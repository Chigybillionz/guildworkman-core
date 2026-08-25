package com.guildworkman.api.booking.service;

import com.guildworkman.api.booking.api.ConfirmReservationRequest;
import com.guildworkman.api.booking.api.ReserveSlotRequest;
import com.guildworkman.api.booking.api.WorkerAvailabilityResponse;
import com.guildworkman.api.booking.api.WorkerAvailabilityResponse.SlotState;
import com.guildworkman.api.booking.api.WorkerAvailabilityResponse.TakenSlot;
import com.guildworkman.api.booking.model.SlotReservation;
import com.guildworkman.api.booking.model.SlotReservationStatus;
import com.guildworkman.api.booking.repository.SlotReservationRepository;
import com.guildworkman.api.data.constants.AppointmentStatus;
import com.guildworkman.api.data.constants.NotificationType;
import com.guildworkman.api.data.models.Appointment;
import com.guildworkman.api.data.models.Client;
import com.guildworkman.api.data.models.SkilledWorker;
import com.guildworkman.api.data.repository.AppointmentRepository;
import com.guildworkman.api.data.repository.ClientRepository;
import com.guildworkman.api.data.repository.SkilledWorkerRepository;
import com.guildworkman.api.exceptions.GuildWorkmanException;
import com.guildworkman.api.exceptions.UserNotFoundException;
import com.guildworkman.api.services.ServiceUtils.NotificationService;
import lombok.RequiredArgsConstructor;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;
import org.springframework.dao.DataIntegrityViolationException;
import org.springframework.data.domain.PageRequest;
import org.springframework.scheduling.annotation.Scheduled;
import org.springframework.stereotype.Service;
import org.springframework.transaction.annotation.Transactional;

import java.time.Duration;
import java.time.Instant;
import java.time.LocalDateTime;
import java.util.ArrayList;
import java.util.HashSet;
import java.util.List;
import java.util.Optional;
import java.util.Set;

/**
 * Public entry point for concurrency-safe appointment booking.
 *
 * <p>The booking flow is reserve → confirm:
 * <ol>
 *   <li>{@link #reserve} takes a short-lived, authoritative hold on the slot, so
 *       a visitor filling in booking details isn't racing anyone for it;</li>
 *   <li>{@link #confirm} turns that hold into an {@code Appointment};</li>
 *   <li>{@link #release} gives it back early, and {@link #expireLapsedHolds()}
 *       reclaims the ones nobody released.</li>
 * </ol>
 * The actual double-booking guard lives in {@link SlotReservationBooker} — see
 * that class for how the lock, the overlap check and the unique index compose.
 *
 * <p>{@link #reserve} is deliberately <b>not</b> {@code @Transactional}. It has
 * to be able to observe the row that won a unique-index race, and a
 * unique-violation marks the surrounding transaction rollback-only — so the
 * recovery read has to happen after that transaction has ended, not inside it.
 * Keeping the transaction boundary at {@link SlotReservationBooker#hold} rather
 * than nesting one (as {@code EscrowOrchestrationInserter} does with
 * {@code REQUIRES_NEW}) also means a booking never holds two pooled connections
 * at once, which matters here because the outer call already holds a row lock
 * for the duration.
 */
@Service
@RequiredArgsConstructor
public class SlotReservationService {

    private static final Logger log = LoggerFactory.getLogger(SlotReservationService.class);

    private final SlotReservationBooker booker;
    private final SlotReservationRepository reservations;
    private final AppointmentRepository appointments;
    private final ClientRepository clients;
    private final SkilledWorkerRepository skilledWorkers;
    private final SlotReservationProperties properties;
    private final NotificationService notificationService;

    /** @param replayed true when {@code reservation} already existed for this idempotency key. */
    public record ReservationOutcome(SlotReservation reservation, boolean replayed) {
    }

    /**
     * Holds a slot, or returns the existing hold for an already-seen
     * idempotency key.
     *
     * @throws SlotUnavailableException if another client got the slot first
     */
    public ReservationOutcome reserve(ReserveSlotRequest request) {
        Optional<SlotReservation> existing = reservations.findByIdempotencyKey(request.idempotencyKey());
        if (existing.isPresent()) {
            return new ReservationOutcome(existing.get(), true);
        }
        try {
            SlotReservation created = booker.hold(request);
            log.info("Slot reservation id={} held (worker={} client={} slot={} expires={})",
                    created.getId(), created.getSkilledWorkerId(), created.getClientId(),
                    created.getSlotStart(), created.getExpiresAt());
            return new ReservationOutcome(created, false);
        } catch (SlotUnavailableException | DataIntegrityViolationException exception) {
            // Something committed between the check above and our own attempt.
            // The one case that isn't a genuine conflict is a concurrent retry
            // of *this same request*: it queued behind us on the worker lock,
            // won, and its row is the answer we were going to produce anyway —
            // so replay it rather than telling the caller they lost a race
            // against themselves. This is why re-reading the key here matters
            // and not just at the top: on that path the retry hadn't committed
            // yet when we first looked.
            //
            // Anything else is a real loss:
            //   - SlotUnavailableException: another client took the slot.
            //   - DataIntegrityViolationException on active_slot_key: a claim
            //     that skipped the per-worker lock. Not reachable today, but
            //     the index exists so it fails loudly instead of double-booking.
            Optional<SlotReservation> ownRetry = reservations.findByIdempotencyKey(request.idempotencyKey());
            if (ownRetry.isPresent()) {
                return new ReservationOutcome(ownRetry.get(), true);
            }
            if (exception instanceof SlotUnavailableException slotUnavailable) {
                throw slotUnavailable;
            }
            throw new SlotUnavailableException(request.skilledWorkerId(),
                    request.slotStart(), request.slotStart().plus(slotDurationOf(request)));
        }
    }

    /**
     * Turns a hold into an appointment.
     *
     * <p>Idempotent: confirming an already-confirmed reservation returns it (and
     * its existing appointment) rather than booking a second one. Two concurrent
     * confirms of the same hold are serialised by
     * {@link SlotReservationRepository#findByIdForUpdate}, so the loser takes
     * that same path.
     */
    @Transactional
    public SlotReservation confirm(Long reservationId, ConfirmReservationRequest request) {
        SlotReservation reservation = reservations.findByIdForUpdate(reservationId)
                .orElseThrow(() -> new ReservationNotFoundException(reservationId));

        if (reservation.getStatus() == SlotReservationStatus.CONFIRMED) {
            return reservation;
        }
        if (reservation.getStatus() != SlotReservationStatus.HELD) {
            throw new ReservationNotHeldException(reservationId, reservation.getStatus());
        }

        Instant now = Instant.now();
        if (reservation.isExpiredHold(now)) {
            // Rejected without writing anything: this method throws, which rolls
            // the transaction back, so marking the row EXPIRED here would be
            // undone anyway. The slot is already effectively free — every query
            // that decides occupancy filters on expiresAt — and the row is
            // relabelled by whichever comes first, the next reserve for this
            // worker or the background sweep.
            throw ReservationNotHeldException.expired(reservationId);
        }

        Appointment appointment = createAppointment(reservation, request);
        reservation.setStatus(SlotReservationStatus.CONFIRMED);
        reservation.setExpiresAt(null);
        reservation.setConfirmedAt(now);
        reservation.setAppointmentId(appointment.getId());
        reservations.save(reservation);

        notificationService.notifyAppointmentEvent(appointment, NotificationType.APPOINTMENT_BOOKED);

        log.info("Slot reservation id={} confirmed as appointment id={}", reservationId, appointment.getId());
        return reservation;
    }

    /**
     * Gives a hold back early. Idempotent for an already-released or expired
     * reservation; a {@code CONFIRMED} one is rejected, because freeing that
     * slot means cancelling the appointment it produced
     * ({@code PUT /api/v1/client/cancelAppointment}), which
     * {@link #releaseForAppointment} then does.
     */
    @Transactional
    public SlotReservation release(Long reservationId) {
        SlotReservation reservation = reservations.findByIdForUpdate(reservationId)
                .orElseThrow(() -> new ReservationNotFoundException(reservationId));

        if (reservation.getStatus() == SlotReservationStatus.CONFIRMED) {
            throw new ReservationNotHeldException(reservationId, reservation.getStatus());
        }
        if (reservation.getStatus() == SlotReservationStatus.HELD) {
            reservation.releaseAs(SlotReservationStatus.RELEASED, Instant.now());
            reservations.save(reservation);
            log.info("Slot reservation id={} released by client", reservationId);
        }
        return reservation;
    }

    @Transactional(readOnly = true)
    public SlotReservation get(Long reservationId) {
        return reservations.findById(reservationId)
                .orElseThrow(() -> new ReservationNotFoundException(reservationId));
    }

    /**
     * Frees the slot behind a cancelled or deleted appointment. Without this a
     * cancelled appointment would leave its {@code CONFIRMED} reservation
     * occupying the calendar forever, and nobody could rebook the time.
     */
    @Transactional
    public void releaseForAppointment(Long appointmentId) {
        List<SlotReservation> confirmed =
                reservations.findByAppointmentIdAndStatus(appointmentId, SlotReservationStatus.CONFIRMED);
        Instant now = Instant.now();
        for (SlotReservation reservation : confirmed) {
            reservation.releaseAs(SlotReservationStatus.RELEASED, now);
            reservations.save(reservation);
            log.info("Slot reservation id={} released because appointment id={} was cancelled or deleted",
                    reservation.getId(), appointmentId);
        }
    }

    /**
     * Everything already claimed on one worker's calendar between {@code from}
     * and {@code to} — confirmed appointments and live holds alike.
     *
     * <p>Merges two sources: reservation rows, and appointments that have no
     * reservation (booked before this feature shipped). An appointment already
     * represented by a {@code CONFIRMED} reservation is reported once, from the
     * reservation, which is the row that carries the real slot end.
     */
    @Transactional(readOnly = true)
    public WorkerAvailabilityResponse availability(Long workerId, LocalDateTime from, LocalDateTime to) {
        if (!skilledWorkers.existsById(workerId)) {
            throw new UserNotFoundException("Skilled worker not found: " + workerId);
        }
        if (!to.isAfter(from)) {
            throw new GuildWorkmanException("'to' must be after 'from'");
        }
        Duration window = Duration.between(from, to);
        if (window.compareTo(properties.getMaxAvailabilityWindow()) > 0) {
            throw new GuildWorkmanException("Availability window must not exceed "
                    + properties.getMaxAvailabilityWindow().toDays() + " days");
        }

        Instant now = Instant.now();
        Duration slotDuration = properties.getSlotDuration();
        List<TakenSlot> taken = new ArrayList<>();
        Set<Long> appointmentIdsCoveredByReservations = new HashSet<>();

        for (SlotReservation reservation : reservations.findOccupyingWindow(
                workerId, from, to, SlotReservationBooker.OCCUPYING_RESERVATION_STATUSES, now)) {
            if (reservation.getAppointmentId() != null) {
                appointmentIdsCoveredByReservations.add(reservation.getAppointmentId());
            }
            taken.add(new TakenSlot(
                    reservation.getSlotStart(),
                    reservation.getSlotEnd(),
                    reservation.getStatus() == SlotReservationStatus.CONFIRMED ? SlotState.BOOKED : SlotState.HELD,
                    reservation.getExpiresAt(),
                    reservation.getId(),
                    reservation.getAppointmentId()));
        }

        for (Appointment appointment : appointments.findOverlapping(workerId, from.minus(slotDuration), to,
                AppointmentStatus.OCCUPYING)) {
            if (appointmentIdsCoveredByReservations.contains(appointment.getId())) {
                continue;
            }
            taken.add(new TakenSlot(
                    appointment.getScheduleTime(),
                    appointment.getScheduleTime().plus(slotDuration),
                    SlotState.BOOKED,
                    null,
                    null,
                    appointment.getId()));
        }

        taken.sort(java.util.Comparator.comparing(TakenSlot::start));
        return new WorkerAvailabilityResponse(workerId, from, to, (int) slotDuration.toMinutes(), taken);
    }

    /**
     * Reclaims holds nobody confirmed or released. The reserve path expires a
     * worker's lapsed holds inline too, so a slot is never blocked by a dead
     * hold in between sweeps — this exists so those rows don't linger as
     * {@code HELD} indefinitely on a quiet calendar, and so the availability
     * read reflects reality without depending on someone attempting a booking
     * first.
     */
    @Scheduled(fixedDelayString = "${booking.expiry-sweep-delay-ms:30000}")
    public void expireLapsedHolds() {
        int expired = booker.expireLapsedHolds(PageRequest.of(0, properties.getExpirySweepBatchSize()));
        if (expired > 0) {
            log.info("Expired {} lapsed slot hold(s)", expired);
        }
    }

    private Appointment createAppointment(SlotReservation reservation, ConfirmReservationRequest request) {
        Client client = clients.findById(reservation.getClientId())
                .orElseThrow(() -> new UserNotFoundException("Client not found: " + reservation.getClientId()));
        SkilledWorker worker = skilledWorkers.findById(reservation.getSkilledWorkerId())
                .orElseThrow(() -> new UserNotFoundException(
                        "Skilled worker not found: " + reservation.getSkilledWorkerId()));

        Appointment appointment = new Appointment();
        appointment.setStatus(AppointmentStatus.SCHEDULED);
        appointment.setScheduleTime(reservation.getSlotStart());
        appointment.setClient(client);
        appointment.setSkilledWorker(worker);
        appointment.setCategory(request != null && request.category() != null
                ? request.category()
                : worker.getCategory());
        if (request != null) {
            appointment.setAmount(request.amount());
        }
        // Saved without touching client.getAppointment()/worker.getAppointment().
        // Those collections are cascade = ALL + orphanRemoval; the appointment's
        // own FK columns are what persist the relation, and mutating the
        // collections here would only risk the cascade hazards documented in
        // ClientServiceImpl#deleteAppointment.
        return appointments.saveAndFlush(appointment);
    }

    private Duration slotDurationOf(ReserveSlotRequest request) {
        return request.durationMinutes() == null
                ? properties.getSlotDuration()
                : Duration.ofMinutes(request.durationMinutes());
    }
}
