package com.guildworkman.api.booking;

import com.guildworkman.api.booking.api.ConfirmReservationRequest;
import com.guildworkman.api.booking.api.ReserveSlotRequest;
import com.guildworkman.api.booking.api.WorkerAvailabilityResponse;
import com.guildworkman.api.booking.api.WorkerAvailabilityResponse.SlotState;
import com.guildworkman.api.booking.model.SlotReservation;
import com.guildworkman.api.booking.model.SlotReservationStatus;
import com.guildworkman.api.booking.repository.SlotReservationRepository;
import com.guildworkman.api.booking.service.ReservationNotHeldException;
import com.guildworkman.api.booking.service.SlotReservationService;
import com.guildworkman.api.booking.service.SlotUnavailableException;
import com.guildworkman.api.data.constants.AppointmentStatus;
import com.guildworkman.api.data.constants.Category;
import com.guildworkman.api.data.models.Appointment;
import com.guildworkman.api.data.models.Client;
import com.guildworkman.api.data.models.SkilledWorker;
import com.guildworkman.api.data.repository.AppointmentRepository;
import com.guildworkman.api.data.repository.ClientRepository;
import com.guildworkman.api.data.repository.SkilledWorkerRepository;
import com.guildworkman.api.dto.requests.BookAppointmentRequest;
import com.guildworkman.api.dto.requests.UpdateAppointmentRequest;
import com.guildworkman.api.exceptions.UserNotFoundException;
import com.guildworkman.api.services.ServiceUtils.ClientService;
import com.guildworkman.api.services.ServiceUtils.MailService;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;
import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.boot.test.context.SpringBootTest;
import org.springframework.boot.test.mock.mockito.MockBean;

import java.math.BigDecimal;
import java.time.Duration;
import java.time.Instant;
import java.time.LocalDateTime;
import java.time.temporal.ChronoUnit;
import java.util.ArrayList;
import java.util.List;
import java.util.UUID;
import java.util.concurrent.Callable;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;

import static org.assertj.core.api.Assertions.assertThat;
import static org.assertj.core.api.Assertions.assertThatThrownBy;

/**
 * Exercises the booking guard against a real Postgres, because that is the only
 * place the guarantee actually lives: the {@code SELECT ... FOR UPDATE} on the
 * worker row and the unique index on {@code active_slot_key} are database
 * behaviour, and a mocked repository would happily "pass" while double-booking
 * in production.
 *
 * <p>{@code spring.jpa.properties.hibernate.jdbc.time_zone} is untouched — slot
 * times are wall-clock {@code LocalDateTime}s, matching {@code Appointment}.
 */
@SpringBootTest(properties = {
        "booking.expiry-sweep-delay-ms=3600000",
        "chain.events.poll-delay-ms=3600000",
        "escrow.orchestration.submit-poll-delay-ms=3600000",
        "escrow.orchestration.confirm-poll-delay-ms=3600000",
        "escrow.reconciliation.poll-delay-ms=3600000"
})
class SlotReservationIntegrationTest {

    @Autowired
    private SlotReservationService slotReservationService;

    @Autowired
    private SlotReservationRepository reservations;

    @Autowired
    private AppointmentRepository appointments;

    @Autowired
    private ClientRepository clients;

    @Autowired
    private SkilledWorkerRepository skilledWorkers;

    @Autowired
    private ClientService clientService;

    // Booking/confirming an appointment now fans out a notification email.
    // Mocked so these tests never make a real call to the mail provider,
    // mirroring how EscrowOrchestrationIntegrationTest mocks SorobanRpcClient.
    @MockBean
    private MailService mailService;

    private SkilledWorker worker;
    private Client client;
    private LocalDateTime slotStart;

    @BeforeEach
    void setUp() {
        reservations.deleteAll();
        // Every test gets its own worker, so the per-worker queries can't see
        // rows another test in this shared database left behind.
        worker = newWorker();
        client = newClient();
        slotStart = LocalDateTime.now().plusDays(1).truncatedTo(ChronoUnit.HOURS);
    }

    private SkilledWorker newWorker() {
        SkilledWorker w = new SkilledWorker();
        w.setFullName("Slot Test Worker");
        w.setEmail("worker-" + UUID.randomUUID() + "@example.com");
        w.setUsername("worker-" + UUID.randomUUID());
        w.setPhoneNumber(UUID.randomUUID().toString());
        w.setCategory(Category.PLUMBING);
        return skilledWorkers.saveAndFlush(w);
    }

    private Client newClient() {
        Client c = new Client();
        c.setFullName("Slot Test Client");
        c.setEmail("client-" + UUID.randomUUID() + "@example.com");
        c.setUsername("client-" + UUID.randomUUID());
        c.setPhoneNumber(UUID.randomUUID().toString());
        return clients.saveAndFlush(c);
    }

    private ReserveSlotRequest reserveRequest(LocalDateTime start, Integer durationMinutes) {
        return new ReserveSlotRequest("idem-" + UUID.randomUUID(), worker.getId(), client.getId(),
                start, durationMinutes);
    }

    // --- the race this issue exists to close ---------------------------------

    @Test
    void concurrentReservationsForTheSameSlotProduceExactlyOneWinner() throws Exception {
        int threads = 12;
        // Each caller is a different visitor with its own idempotency key, so
        // nothing but the slot guard can dedupe them.
        List<ReserveSlotRequest> requests = new ArrayList<>();
        for (int i = 0; i < threads; i++) {
            requests.add(reserveRequest(slotStart, null));
        }

        AtomicInteger conflicts = new AtomicInteger();
        List<Long> winners = runConcurrently(threads, requests, conflicts);

        assertThat(winners).hasSize(1);
        assertThat(conflicts.get()).isEqualTo(threads - 1);
        assertThat(reservations.count()).isEqualTo(1);
    }

    @Test
    void concurrentReservationsForDifferentWorkersDoNotContend() throws Exception {
        SkilledWorker otherWorker = newWorker();
        int threads = 8;
        List<ReserveSlotRequest> requests = new ArrayList<>();
        for (int i = 0; i < threads; i++) {
            Long workerId = i % 2 == 0 ? worker.getId() : otherWorker.getId();
            requests.add(new ReserveSlotRequest("idem-" + UUID.randomUUID(), workerId, client.getId(),
                    slotStart, null));
        }

        AtomicInteger conflicts = new AtomicInteger();
        List<Long> winners = runConcurrently(threads, requests, conflicts);

        // One winner per worker: the lock is per worker, not global.
        assertThat(winners).hasSize(2);
        assertThat(conflicts.get()).isEqualTo(threads - 2);
    }

    @Test
    void concurrentOneStepBookingsForTheSameSlotProduceExactlyOneAppointment() throws Exception {
        int threads = 8;
        ExecutorService pool = Executors.newFixedThreadPool(threads);
        CountDownLatch startLine = new CountDownLatch(1);
        AtomicInteger booked = new AtomicInteger();
        AtomicInteger conflicts = new AtomicInteger();

        List<Callable<Void>> tasks = new ArrayList<>();
        for (int i = 0; i < threads; i++) {
            tasks.add(() -> {
                startLine.await();
                BookAppointmentRequest request = new BookAppointmentRequest();
                request.setClientId(client.getId());
                request.setSkilledWorkerId(worker.getId());
                request.setScheduleTime(slotStart);
                request.setCategory(Category.PLUMBING);
                request.setAmount(BigDecimal.TEN);
                try {
                    clientService.bookAppointment(request);
                    booked.incrementAndGet();
                } catch (SlotUnavailableException expected) {
                    conflicts.incrementAndGet();
                }
                return null;
            });
        }

        List<Future<Void>> futures = new ArrayList<>();
        for (Callable<Void> task : tasks) {
            futures.add(pool.submit(task));
        }
        startLine.countDown();
        for (Future<Void> future : futures) {
            future.get(30, TimeUnit.SECONDS);
        }
        pool.shutdown();
        assertThat(pool.awaitTermination(10, TimeUnit.SECONDS)).isTrue();

        // The legacy one-step endpoint goes through the same guard, so it can't
        // be used to sidestep it.
        assertThat(booked.get()).isEqualTo(1);
        assertThat(conflicts.get()).isEqualTo(threads - 1);
        assertThat(appointmentsFor(worker)).hasSize(1);
    }

    private List<Long> runConcurrently(int threads, List<ReserveSlotRequest> requests, AtomicInteger conflicts)
            throws Exception {
        ExecutorService pool = Executors.newFixedThreadPool(threads);
        CountDownLatch startLine = new CountDownLatch(1);
        List<Future<Long>> futures = new ArrayList<>();
        for (ReserveSlotRequest request : requests) {
            futures.add(pool.submit(() -> {
                startLine.await();
                try {
                    return slotReservationService.reserve(request).reservation().getId();
                } catch (SlotUnavailableException expected) {
                    conflicts.incrementAndGet();
                    return null;
                }
            }));
        }
        startLine.countDown();

        List<Long> winners = new ArrayList<>();
        for (Future<Long> future : futures) {
            Long id = future.get(30, TimeUnit.SECONDS);
            if (id != null) {
                winners.add(id);
            }
        }
        pool.shutdown();
        assertThat(pool.awaitTermination(10, TimeUnit.SECONDS)).isTrue();
        return winners;
    }

    // --- overlap semantics ----------------------------------------------------

    @Test
    void aPartiallyOverlappingSlotIsRejected() {
        slotReservationService.reserve(reserveRequest(slotStart, 60));

        // Different start, so the active_slot_key index can't catch it — the
        // overlap query under the worker lock has to.
        assertThatThrownBy(() -> slotReservationService.reserve(reserveRequest(slotStart.plusMinutes(30), 60)))
                .isInstanceOf(SlotUnavailableException.class);
    }

    @Test
    void anAdjacentSlotIsAccepted() {
        slotReservationService.reserve(reserveRequest(slotStart, 60));

        // [start, start+60) and [start+60, start+120) share only the boundary
        // instant, which the half-open interval excludes.
        var next = slotReservationService.reserve(reserveRequest(slotStart.plusMinutes(60), 60));
        assertThat(next.reservation().getStatus()).isEqualTo(SlotReservationStatus.HELD);
    }

    @Test
    void anExistingAppointmentWithNoReservationStillBlocksTheSlot() {
        // Mimics a row booked before this feature shipped: an appointment with
        // no slot_reservations row behind it.
        Appointment legacy = new Appointment();
        legacy.setStatus(AppointmentStatus.SCHEDULED);
        legacy.setScheduleTime(slotStart);
        legacy.setSkilledWorker(worker);
        legacy.setClient(client);
        appointments.saveAndFlush(legacy);

        assertThatThrownBy(() -> slotReservationService.reserve(reserveRequest(slotStart, null)))
                .isInstanceOf(SlotUnavailableException.class);
    }

    @Test
    void aCancelledAppointmentNoLongerBlocksTheSlot() {
        Long appointmentId = bookOneStep(slotStart);
        clientService.cancelAppointment(appointmentId);

        // The reservation went back too — otherwise the slot would be blocked
        // forever by an appointment nobody is attending.
        var rebooked = slotReservationService.reserve(reserveRequest(slotStart, null));
        assertThat(rebooked.reservation().getStatus()).isEqualTo(SlotReservationStatus.HELD);
    }

    @Test
    void reschedulingAnAppointmentClaimsTheNewSlotAndGivesTheOldOneBack() {
        Long appointmentId = bookOneStep(slotStart);

        UpdateAppointmentRequest reschedule = new UpdateAppointmentRequest();
        reschedule.setStatus(AppointmentStatus.UPDATED);
        reschedule.setStartTime(slotStart.plusHours(3));
        clientService.updateAppointment(appointmentId, reschedule);

        // The old time is bookable again...
        assertThat(slotReservationService.reserve(reserveRequest(slotStart, null)).reservation().getStatus())
                .isEqualTo(SlotReservationStatus.HELD);
        // ...and the new one isn't.
        assertThatThrownBy(() -> slotReservationService.reserve(reserveRequest(slotStart.plusHours(3), null)))
                .isInstanceOf(SlotUnavailableException.class);
    }

    @Test
    void reschedulingOntoATakenSlotIsRejected() {
        Long appointmentId = bookOneStep(slotStart);
        slotReservationService.reserve(reserveRequest(slotStart.plusHours(2), null));

        UpdateAppointmentRequest reschedule = new UpdateAppointmentRequest();
        reschedule.setStatus(AppointmentStatus.UPDATED);
        reschedule.setStartTime(slotStart.plusHours(2));

        // Rescheduling is a booking of a different slot, so it goes through the
        // same guard rather than around it.
        assertThatThrownBy(() -> clientService.updateAppointment(appointmentId, reschedule))
                .isInstanceOf(SlotUnavailableException.class);
    }

    @Test
    void aSmallReschedulingShiftDoesNotConflictWithTheAppointmentItself() {
        Long appointmentId = bookOneStep(slotStart);

        UpdateAppointmentRequest reschedule = new UpdateAppointmentRequest();
        reschedule.setStatus(AppointmentStatus.UPDATED);
        // Overlaps its own old slot — the appointment must be excluded from its
        // own conflict check.
        reschedule.setStartTime(slotStart.plusMinutes(30));

        clientService.updateAppointment(appointmentId, reschedule);

        assertThat(appointments.findById(appointmentId).orElseThrow().getScheduleTime())
                .isEqualTo(slotStart.plusMinutes(30));
    }

    @Test
    void decliningAnAppointmentFreesItsSlot() {
        Long appointmentId = bookOneStep(slotStart);

        UpdateAppointmentRequest decline = new UpdateAppointmentRequest();
        decline.setStatus(AppointmentStatus.DECLINED);
        clientService.updateAppointment(appointmentId, decline);

        assertThat(slotReservationService.reserve(reserveRequest(slotStart, null)).reservation().getStatus())
                .isEqualTo(SlotReservationStatus.HELD);
    }

    @Test
    void reservingForAnUnknownWorkerIsNotFound() {
        ReserveSlotRequest request = new ReserveSlotRequest("idem-" + UUID.randomUUID(), 9_999_999L,
                client.getId(), slotStart, null);
        assertThatThrownBy(() -> slotReservationService.reserve(request))
                .isInstanceOf(UserNotFoundException.class);
    }

    // --- idempotency -----------------------------------------------------------

    @Test
    void resendingAnIdempotencyKeyReplaysTheOriginalHold() {
        ReserveSlotRequest request = reserveRequest(slotStart, null);

        var first = slotReservationService.reserve(request);
        var second = slotReservationService.reserve(request);

        assertThat(first.replayed()).isFalse();
        assertThat(second.replayed()).isTrue();
        assertThat(second.reservation().getId()).isEqualTo(first.reservation().getId());
        assertThat(reservations.count()).isEqualTo(1);
    }

    @Test
    void concurrentRetriesOfTheSameRequestTakeOnlyOneHold() throws Exception {
        ReserveSlotRequest request = reserveRequest(slotStart, null);
        int threads = 8;
        ExecutorService pool = Executors.newFixedThreadPool(threads);
        CountDownLatch startLine = new CountDownLatch(1);

        List<Future<Long>> futures = new ArrayList<>();
        for (int i = 0; i < threads; i++) {
            futures.add(pool.submit(() -> {
                startLine.await();
                return slotReservationService.reserve(request).reservation().getId();
            }));
        }
        startLine.countDown();

        List<Long> ids = new ArrayList<>();
        for (Future<Long> future : futures) {
            ids.add(future.get(30, TimeUnit.SECONDS));
        }
        pool.shutdown();
        assertThat(pool.awaitTermination(10, TimeUnit.SECONDS)).isTrue();

        // Every caller sees the same hold; nobody gets a 409, because a retry of
        // your own request isn't a conflict.
        assertThat(ids).doesNotContainNull();
        assertThat(ids.stream().distinct()).hasSize(1);
        assertThat(reservations.count()).isEqualTo(1);
    }

    // --- confirm / release ------------------------------------------------------

    @Test
    void confirmingAHoldCreatesTheAppointment() {
        var held = slotReservationService.reserve(reserveRequest(slotStart, null)).reservation();

        SlotReservation confirmed = slotReservationService.confirm(held.getId(),
                new ConfirmReservationRequest(Category.ELECTRICAL, new BigDecimal("250.00")));

        assertThat(confirmed.getStatus()).isEqualTo(SlotReservationStatus.CONFIRMED);
        assertThat(confirmed.getExpiresAt()).isNull();
        assertThat(confirmed.getAppointmentId()).isNotNull();

        Appointment appointment = appointments.findById(confirmed.getAppointmentId()).orElseThrow();
        assertThat(appointment.getStatus()).isEqualTo(AppointmentStatus.SCHEDULED);
        assertThat(appointment.getScheduleTime()).isEqualTo(slotStart);
        assertThat(appointment.getSkilledWorker().getId()).isEqualTo(worker.getId());
        assertThat(appointment.getClient().getId()).isEqualTo(client.getId());
        assertThat(appointment.getCategory()).isEqualTo(Category.ELECTRICAL);
    }

    @Test
    void confirmingTwiceBooksOnlyOneAppointment() {
        var held = slotReservationService.reserve(reserveRequest(slotStart, null)).reservation();

        var first = slotReservationService.confirm(held.getId(), null);
        var second = slotReservationService.confirm(held.getId(), null);

        assertThat(second.getAppointmentId()).isEqualTo(first.getAppointmentId());
        assertThat(appointmentsFor(worker)).hasSize(1);
    }

    @Test
    void confirmingFallsBackToTheWorkersOwnCategory() {
        var held = slotReservationService.reserve(reserveRequest(slotStart, null)).reservation();

        var confirmed = slotReservationService.confirm(held.getId(), new ConfirmReservationRequest(null, null));

        Appointment appointment = appointments.findById(confirmed.getAppointmentId()).orElseThrow();
        assertThat(appointment.getCategory()).isEqualTo(Category.PLUMBING);
    }

    @Test
    void confirmingALapsedHoldIsRejectedAndTheSlotGoesBack() {
        var held = slotReservationService.reserve(reserveRequest(slotStart, null)).reservation();
        expire(held.getId());

        assertThatThrownBy(() -> slotReservationService.confirm(held.getId(), null))
                .isInstanceOf(ReservationNotHeldException.class);

        // The rejected confirm books nothing, and somebody else can take the
        // slot straight away — the lapsed row is relabelled by the reserve that
        // steps over it.
        assertThat(slotReservationService.reserve(reserveRequest(slotStart, null)).reservation().getStatus())
                .isEqualTo(SlotReservationStatus.HELD);
        assertThat(reservations.findById(held.getId()).orElseThrow().getStatus())
                .isEqualTo(SlotReservationStatus.EXPIRED);
    }

    @Test
    void releasingAHoldFreesTheSlotImmediately() {
        var held = slotReservationService.reserve(reserveRequest(slotStart, null)).reservation();

        slotReservationService.release(held.getId());

        assertThat(reservations.findById(held.getId()).orElseThrow().getStatus())
                .isEqualTo(SlotReservationStatus.RELEASED);
        assertThat(slotReservationService.reserve(reserveRequest(slotStart, null)).reservation().getStatus())
                .isEqualTo(SlotReservationStatus.HELD);
    }

    @Test
    void releasingAConfirmedReservationIsRejected() {
        var held = slotReservationService.reserve(reserveRequest(slotStart, null)).reservation();
        slotReservationService.confirm(held.getId(), null);

        assertThatThrownBy(() -> slotReservationService.release(held.getId()))
                .isInstanceOf(ReservationNotHeldException.class);
    }

    // --- expiry ------------------------------------------------------------------

    @Test
    void aLapsedHoldStopsBlockingTheSlotEvenBeforeTheSweepRuns() {
        var held = slotReservationService.reserve(reserveRequest(slotStart, null)).reservation();
        expire(held.getId());

        var second = slotReservationService.reserve(reserveRequest(slotStart, null));

        assertThat(second.reservation().getId()).isNotEqualTo(held.getId());
        assertThat(reservations.findById(held.getId()).orElseThrow().getStatus())
                .isEqualTo(SlotReservationStatus.EXPIRED);
    }

    @Test
    void theBackgroundSweepRetiresLapsedHolds() {
        var held = slotReservationService.reserve(reserveRequest(slotStart, null)).reservation();
        expire(held.getId());

        slotReservationService.expireLapsedHolds();

        SlotReservation reloaded = reservations.findById(held.getId()).orElseThrow();
        assertThat(reloaded.getStatus()).isEqualTo(SlotReservationStatus.EXPIRED);
        assertThat(reloaded.getActiveSlotKey()).isNull();
    }

    @Test
    void theSweepLeavesLiveHoldsAlone() {
        var held = slotReservationService.reserve(reserveRequest(slotStart, null)).reservation();

        slotReservationService.expireLapsedHolds();

        assertThat(reservations.findById(held.getId()).orElseThrow().getStatus())
                .isEqualTo(SlotReservationStatus.HELD);
    }

    // --- availability ---------------------------------------------------------------

    @Test
    void availabilityReportsBookedAndHeldSlotsForTheWorker() {
        var booked = slotReservationService.reserve(reserveRequest(slotStart, 60)).reservation();
        slotReservationService.confirm(booked.getId(), null);
        slotReservationService.reserve(reserveRequest(slotStart.plusHours(2), 60));

        WorkerAvailabilityResponse availability = slotReservationService.availability(
                worker.getId(), slotStart.minusHours(1), slotStart.plusHours(6));

        assertThat(availability.workerId()).isEqualTo(worker.getId());
        assertThat(availability.taken()).hasSize(2);
        assertThat(availability.taken().get(0).start()).isEqualTo(slotStart);
        assertThat(availability.taken().get(0).state()).isEqualTo(SlotState.BOOKED);
        assertThat(availability.taken().get(0).appointmentId()).isNotNull();
        assertThat(availability.taken().get(1).start()).isEqualTo(slotStart.plusHours(2));
        assertThat(availability.taken().get(1).state()).isEqualTo(SlotState.HELD);
        assertThat(availability.taken().get(1).holdExpiresAt()).isNotNull();
    }

    @Test
    void availabilityReportsAConfirmedSlotOnceNotTwice() {
        var held = slotReservationService.reserve(reserveRequest(slotStart, 60)).reservation();
        slotReservationService.confirm(held.getId(), null);

        // The reservation and the appointment it produced describe the same
        // slot; only one entry should come back.
        WorkerAvailabilityResponse availability = slotReservationService.availability(
                worker.getId(), slotStart.minusHours(1), slotStart.plusHours(3));

        assertThat(availability.taken()).hasSize(1);
    }

    @Test
    void availabilityIgnoresLapsedHoldsAndOtherWorkers() {
        SkilledWorker otherWorker = newWorker();
        slotReservationService.reserve(new ReserveSlotRequest("idem-" + UUID.randomUUID(), otherWorker.getId(),
                client.getId(), slotStart, null));
        var lapsed = slotReservationService.reserve(reserveRequest(slotStart.plusHours(3), null)).reservation();
        expire(lapsed.getId());

        WorkerAvailabilityResponse availability = slotReservationService.availability(
                worker.getId(), slotStart.minusHours(1), slotStart.plusHours(6));

        assertThat(availability.taken()).isEmpty();
    }

    @Test
    void availabilityIncludesAppointmentsThatHaveNoReservation() {
        Appointment legacy = new Appointment();
        legacy.setStatus(AppointmentStatus.ACCEPTED);
        legacy.setScheduleTime(slotStart);
        legacy.setSkilledWorker(worker);
        legacy.setClient(client);
        appointments.saveAndFlush(legacy);

        WorkerAvailabilityResponse availability = slotReservationService.availability(
                worker.getId(), slotStart.minusHours(1), slotStart.plusHours(3));

        assertThat(availability.taken()).hasSize(1);
        assertThat(availability.taken().get(0).state()).isEqualTo(SlotState.BOOKED);
        // No stored end time on a legacy appointment, so the default slot
        // duration supplies one.
        assertThat(availability.taken().get(0).end())
                .isEqualTo(slotStart.plusMinutes(availability.slotDurationMinutes()));
    }

    @Test
    void availabilityForAnUnknownWorkerIsNotFound() {
        assertThatThrownBy(() -> slotReservationService.availability(9_999_999L, slotStart, slotStart.plusDays(1)))
                .isInstanceOf(UserNotFoundException.class);
    }

    /** Books through the one-step endpoint and returns the appointment's id. */
    private Long bookOneStep(LocalDateTime start) {
        BookAppointmentRequest request = new BookAppointmentRequest();
        request.setClientId(client.getId());
        request.setSkilledWorkerId(worker.getId());
        request.setScheduleTime(start);
        request.setCategory(Category.PLUMBING);
        clientService.bookAppointment(request);
        return appointmentsFor(worker).get(0).getId();
    }

    /** Backdates a hold's TTL so the lapse path can be tested without waiting five minutes. */
    private void expire(Long reservationId) {
        SlotReservation reservation = reservations.findById(reservationId).orElseThrow();
        reservation.setExpiresAt(Instant.now().minus(Duration.ofMinutes(1)));
        reservations.saveAndFlush(reservation);
    }

    private List<Appointment> appointmentsFor(SkilledWorker skilledWorker) {
        return appointments.findOverlapping(skilledWorker.getId(),
                slotStart.minusDays(1), slotStart.plusDays(1),
                List.of(AppointmentStatus.SCHEDULED, AppointmentStatus.ACCEPTED, AppointmentStatus.UPDATED));
    }
}
