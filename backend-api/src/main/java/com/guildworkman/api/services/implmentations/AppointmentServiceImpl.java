package com.guildworkman.api.services.implmentations;

import com.guildworkman.api.dto.requests.AcceptAppointmentRequest;
import com.guildworkman.api.dto.requests.BookAppointmentRequest;
import com.guildworkman.api.dto.requests.UpdateAppointmentRequest;
import com.guildworkman.api.dto.responses.AcceptAppointmentResponse;
import com.guildworkman.api.dto.responses.UpdateAppointmentResponse;
import com.guildworkman.api.dto.responses.ViewAllAppointmentsResponse;
import com.guildworkman.api.exceptions.AppointmentNotFoundException;
import com.guildworkman.api.data.constants.NotificationType;
import com.guildworkman.api.data.models.Appointment;
import com.guildworkman.api.data.models.Client;
import com.guildworkman.api.data.models.SkilledWorker;
import com.guildworkman.api.data.constants.AppointmentStatus;
import com.guildworkman.api.data.repository.AppointmentRepository;
import com.guildworkman.api.data.repository.ClientRepository;
import com.guildworkman.api.data.repository.SkilledWorkerRepository;
import com.guildworkman.api.exceptions.GuildWorkmanException;
import com.guildworkman.api.exceptions.UserNotFoundException;
import com.guildworkman.api.booking.model.SlotReservation;
import com.guildworkman.api.booking.service.SlotReservationBooker;
import com.guildworkman.api.booking.service.SlotReservationService;
import com.guildworkman.api.services.ServiceUtils.AppointmentService;
import com.guildworkman.api.services.ServiceUtils.NotificationService;
import lombok.AllArgsConstructor;
import org.modelmapper.ModelMapper;
import org.springframework.stereotype.Service;
import org.springframework.transaction.annotation.Transactional;

import java.time.LocalDateTime;
import java.util.List;
import java.util.Optional;
import java.util.UUID;

@Service
@AllArgsConstructor
public class AppointmentServiceImpl implements AppointmentService {

    /**
     * Marks the idempotency keys minted for one-step bookings, which have no
     * client-supplied key of their own. Random per call: this endpoint has no
     * way to tell a retry from a genuine second booking, so every call is a new
     * claim (and a duplicate one loses to the slot guard, not to the key).
     */
    private static final String LEGACY_BOOKING_KEY_PREFIX = "book-appointment-";

    /** Same rationale as {@link #LEGACY_BOOKING_KEY_PREFIX}, for slot changes. */
    private static final String RESCHEDULE_KEY_PREFIX = "reschedule-appointment-";

    private final AppointmentRepository appointmentRepository;
    private  final ModelMapper modelMapper;
    private final SkilledWorkerRepository skilledWorkerRepository;
    private final ClientRepository clientRepository;
    private final SlotReservationBooker slotReservationBooker;
    private final SlotReservationService slotReservationService;
    private final NotificationService notificationService;


//    @Autowired
//    public void setClientService(ClientService clientService) {
//        this.clientService = clientService;
//    }

    @Override
    @Transactional
    public Appointment bookAppointment(BookAppointmentRequest bookAppointmentRequest) {
        Appointment appointment = modelMapper.map(bookAppointmentRequest, Appointment.class);
        appointment.setStatus(AppointmentStatus.SCHEDULED);

        // Record WHICH worker was booked. Set explicitly rather than relying on
        // ModelMapper, which can implicitly build a transient SkilledWorker from
        // skilledWorkerId — save that and Hibernate blows up. Resolve the managed
        // entity (or clear it) before persisting.
        SkilledWorker skilledWorker = resolveSkilledWorker(bookAppointmentRequest.getSkilledWorkerId());
        appointment.setSkilledWorker(skilledWorker);
        appointment.setAmount(bookAppointmentRequest.getAmount());

        // Same reasoning as the worker above: ModelMapper maps `clientId` onto a
        // transient Client with only its id set (everything else null), which
        // would either fail to persist or silently notify with a blank name/email.
        // Resolve the managed entity instead.
        Client client = clientRepository.findById(bookAppointmentRequest.getClientId())
                .orElseThrow(() -> new UserNotFoundException(
                        "Client not found: " + bookAppointmentRequest.getClientId()));
        appointment.setClient(client);

        // Claim the slot BEFORE the appointment exists. This one-step endpoint
        // races just as hard as the two-step reserve/confirm flow does, so it
        // goes through the same per-worker lock and overlap check — otherwise
        // it would be a way around the guard, and the availability read would
        // under-report. Claiming first also matters for correctness: an
        // appointment saved first would show up in the claim's own overlap
        // query and conflict with itself.
        //
        // Only bookings that name a worker can be guarded: without one there is
        // no calendar to double-book, and the appointment records no worker at
        // all (see BookAppointmentRequest#skilledWorkerId).
        SlotReservation reservation = skilledWorker == null ? null : slotReservationBooker.claimConfirmed(
                skilledWorker.getId(),
                bookAppointmentRequest.getClientId(),
                bookAppointmentRequest.getScheduleTime(),
                null,
                LEGACY_BOOKING_KEY_PREFIX + UUID.randomUUID());

        appointmentRepository.save(appointment);

        if (reservation != null) {
            slotReservationBooker.linkAppointment(reservation, appointment.getId());
        }

        notificationService.notifyAppointmentEvent(appointment, NotificationType.APPOINTMENT_BOOKED);

        return appointment;

    }

    private SkilledWorker resolveSkilledWorker(Long skilledWorkerId) {
        // Optional: callers that don't send a worker still book successfully,
        // they just produce an appointment with no worker attached.
        if (skilledWorkerId == null) {
            return null;
        }
        return skilledWorkerRepository.findById(skilledWorkerId)
                .orElseThrow(() -> new UserNotFoundException(
                        "Skilled worker not found: " + skilledWorkerId));
    }

    /**
     * Claims the new slot for a rescheduled appointment, giving the old one
     * back. Throws {@code SlotUnavailableException} (409) if the new time is
     * already taken, which leaves the whole update rolled back — a reschedule
     * that can't get the slot must not half-apply.
     *
     * <p>The appointment is excluded from its own overlap check: it still
     * carries its old {@code scheduleTime} at this point, so a small shift
     * (10:00 to 10:30) would otherwise conflict with itself.
     */
    private void moveToSlot(Appointment appointment, LocalDateTime newStart) {
        if (appointment.getSkilledWorker() == null || appointment.getClient() == null) {
            // Nothing to double-book without a worker, and no client to hold it.
            return;
        }
        slotReservationService.releaseForAppointment(appointment.getId());
        SlotReservation moved = slotReservationBooker.claimConfirmed(
                appointment.getSkilledWorker().getId(),
                appointment.getClient().getId(),
                newStart,
                null,
                RESCHEDULE_KEY_PREFIX + UUID.randomUUID(),
                appointment.getId());
        slotReservationBooker.linkAppointment(moved, appointment.getId());
    }



    @Override
    @Transactional
    public void cancelAppointment(Long id) {
        Appointment appointment = appointmentRepository.findById(id)
                .orElseThrow(() -> new AppointmentNotFoundException("Appointment not found"));
        appointment.setStatus(AppointmentStatus.CANCELLED);
        appointmentRepository.save(appointment);
        notificationService.notifyAppointmentEvent(appointment, NotificationType.APPOINTMENT_CANCELLED);
//        CancelAppointmentResponse response = new CancelAppointmentResponse();
//        modelMapper.map(response, Appointment.class);
//        response.setMessage("Appointment cancelled successfully");
//        return response;

    }

    @Override
    @Transactional
    public UpdateAppointmentResponse updateAppointment(Long Id,UpdateAppointmentRequest request) {
        Appointment appointment = appointmentRepository.findById(Id)
                .orElseThrow(()-> new AppointmentNotFoundException("No appointment found"));

        // Apply what the caller actually asked for. This used to map the request
        // into a THROWAWAY Appointment (the result was discarded) and then hardcode
        // UPDATED — so ACCEPTED/DECLINED never reached the database and accepting or
        // declining a job did nothing at all.
        appointment.setStatus(request.getStatus() != null
                ? request.getStatus()
                : AppointmentStatus.UPDATED);
        if (request.getAmount() != null) {
            appointment.setAmount(request.getAmount());
        }

        if (!appointment.getStatus().occupiesSlot()) {
            // DECLINED or CANCELLED via this endpoint frees the worker's time
            // just as much as /cancelAppointment does, so the reservation has to
            // go back here too — otherwise declining a job would leave the slot
            // blocked forever.
            slotReservationService.releaseForAppointment(appointment.getId());
        } else if (request.getStartTime() != null
                && !request.getStartTime().equals(appointment.getScheduleTime())) {
            // Rescheduling is a booking of a different slot and has to survive
            // the same guard, or this endpoint becomes the way around it.
            moveToSlot(appointment, request.getStartTime());
        }

        if (request.getStartTime() != null) {
            appointment.setScheduleTime(request.getStartTime());
        }
        appointmentRepository.save(appointment);

        notificationService.notifyAppointmentEvent(appointment,
                NotificationType.fromAppointmentStatus(appointment.getStatus()));

        UpdateAppointmentResponse response = new UpdateAppointmentResponse();
        response.setId(appointment.getId());
        response.setMessage("Appointment Updated");
        return response;
    }

    @Override
    @Transactional
    public void deleteAppointment(Long id) {
        Appointment appointment = appointmentRepository.findById(id)
                .orElseThrow(()-> new AppointmentNotFoundException("No appointment found"));
        appointmentRepository.delete(appointment);
        notificationService.notifyAppointmentEvent(appointment, NotificationType.APPOINTMENT_DELETED);

    }

    @Override
    public List<ViewAllAppointmentsResponse> viewAllAppointment() {
        var  appointments = appointmentRepository.findAll();

        return List.of(modelMapper
                .map(appointments, ViewAllAppointmentsResponse[].class));
    }

    @Override
    public Optional<Appointment> findAppointmentById(Long id) {
        return appointmentRepository.findById(id);

    }




    @Override
    public void save(Appointment appointment) {
        appointmentRepository.save(appointment);

    }

    @Override
    @Transactional
    public AcceptAppointmentResponse acceptAppointment(AcceptAppointmentRequest request) {
        Appointment appointment = appointmentRepository.findById(request.getAppointmentId())
                .orElseThrow(()-> new GuildWorkmanException("appointment not found"));
//        appointment.setClient(clientService.findById(request.getClientId()));
//        appointment.setSkilledWorker(request.getSkilledWorker());
        appointment.setStatus(AppointmentStatus.ACCEPTED);
        appointmentRepository.save(appointment);
        notificationService.notifyAppointmentEvent(appointment, NotificationType.APPOINTMENT_ACCEPTED);
        AcceptAppointmentResponse response = new AcceptAppointmentResponse();
        response.setStatus(request.getStatus());
        response.setClientId(request.getId());
        response.setAppointmentId(request.getAppointmentId());
        return response;
    }

    @Override
    public Long getAppointments() {
        return appointmentRepository.count();
    }


}
