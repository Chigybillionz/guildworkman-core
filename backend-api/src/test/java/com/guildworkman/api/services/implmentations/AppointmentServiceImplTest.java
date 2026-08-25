package com.guildworkman.api.services.implmentations;

import com.guildworkman.api.booking.service.SlotReservationBooker;
import com.guildworkman.api.booking.service.SlotReservationService;
import com.guildworkman.api.data.constants.AppointmentStatus;
import com.guildworkman.api.data.constants.Category;
import com.guildworkman.api.data.constants.NotificationType;
import com.guildworkman.api.data.models.Appointment;
import com.guildworkman.api.data.models.Client;
import com.guildworkman.api.data.models.SkilledWorker;
import com.guildworkman.api.data.repository.AppointmentRepository;
import com.guildworkman.api.data.repository.ClientRepository;
import com.guildworkman.api.data.repository.SkilledWorkerRepository;
import com.guildworkman.api.dto.requests.AcceptAppointmentRequest;
import com.guildworkman.api.dto.requests.BookAppointmentRequest;
import com.guildworkman.api.dto.requests.UpdateAppointmentRequest;
import com.guildworkman.api.exceptions.AppointmentNotFoundException;
import com.guildworkman.api.exceptions.UserNotFoundException;
import com.guildworkman.api.services.ServiceUtils.NotificationService;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;
import org.modelmapper.ModelMapper;

import java.time.LocalDateTime;
import java.util.Optional;

import static org.assertj.core.api.Assertions.assertThat;
import static org.assertj.core.api.Assertions.assertThatThrownBy;
import static org.mockito.ArgumentMatchers.any;
import static org.mockito.Mockito.mock;
import static org.mockito.Mockito.never;
import static org.mockito.Mockito.verify;
import static org.mockito.Mockito.verifyNoInteractions;
import static org.mockito.Mockito.when;

/**
 * Verifies that every appointment lifecycle transition
 * (booked/accepted/declined/updated/cancelled/deleted) produces exactly the
 * right {@link NotificationType} — the wiring {@code AppointmentServiceImpl}
 * is responsible for, as opposed to the notification content itself, which
 * {@code NotificationServiceImplTest} covers.
 */
class AppointmentServiceImplTest {

    private AppointmentRepository appointmentRepository;
    private SkilledWorkerRepository skilledWorkerRepository;
    private ClientRepository clientRepository;
    private SlotReservationBooker slotReservationBooker;
    private SlotReservationService slotReservationService;
    private NotificationService notificationService;
    private AppointmentServiceImpl service;

    @BeforeEach
    void setUp() {
        appointmentRepository = mock(AppointmentRepository.class);
        skilledWorkerRepository = mock(SkilledWorkerRepository.class);
        clientRepository = mock(ClientRepository.class);
        slotReservationBooker = mock(SlotReservationBooker.class);
        slotReservationService = mock(SlotReservationService.class);
        notificationService = mock(NotificationService.class);

        when(appointmentRepository.save(any())).thenAnswer(invocation -> invocation.getArgument(0));

        service = new AppointmentServiceImpl(appointmentRepository, new ModelMapper(),
                skilledWorkerRepository, clientRepository, slotReservationBooker,
                slotReservationService, notificationService);
    }

    private static BookAppointmentRequest bookingRequest(Long clientId) {
        BookAppointmentRequest request = new BookAppointmentRequest();
        request.setClientId(clientId);
        request.setScheduleTime(LocalDateTime.now().plusDays(1));
        request.setCategory(Category.PLUMBING);
        return request;
    }

    private static Client client(Long id) {
        Client client = new Client();
        client.setId(id);
        client.setEmail("client" + id + "@test.com");
        client.setFullName("Client " + id);
        return client;
    }

    private static SkilledWorker worker(Long id) {
        SkilledWorker worker = new SkilledWorker();
        worker.setId(id);
        worker.setEmail("worker" + id + "@test.com");
        worker.setFullName("Worker " + id);
        return worker;
    }

    private static Appointment existingAppointment(Long id, AppointmentStatus status) {
        Appointment appointment = new Appointment();
        appointment.setId(id);
        appointment.setStatus(status);
        appointment.setScheduleTime(LocalDateTime.now().plusDays(1));
        appointment.setClient(client(100L));
        appointment.setSkilledWorker(worker(200L));
        return appointment;
    }

    // --- bookAppointment -------------------------------------------------------

    @Test
    void bookAppointmentResolvesClientAndNotifiesBooked() {
        BookAppointmentRequest request = bookingRequest(3L);
        Client client = client(3L);
        when(clientRepository.findById(3L)).thenReturn(Optional.of(client));

        Appointment result = service.bookAppointment(request);

        assertThat(result.getClient()).isSameAs(client);
        assertThat(result.getStatus()).isEqualTo(AppointmentStatus.SCHEDULED);
        verify(notificationService).notifyAppointmentEvent(result, NotificationType.APPOINTMENT_BOOKED);
    }

    @Test
    void bookAppointmentThrowsWhenClientNotFound() {
        BookAppointmentRequest request = bookingRequest(404L);
        when(clientRepository.findById(404L)).thenReturn(Optional.empty());

        assertThatThrownBy(() -> service.bookAppointment(request)).isInstanceOf(UserNotFoundException.class);
        verifyNoInteractions(notificationService);
    }

    // --- cancelAppointment -------------------------------------------------------

    @Test
    void cancelAppointmentMarksCancelledAndNotifies() {
        Appointment appointment = existingAppointment(5L, AppointmentStatus.SCHEDULED);
        when(appointmentRepository.findById(5L)).thenReturn(Optional.of(appointment));

        service.cancelAppointment(5L);

        assertThat(appointment.getStatus()).isEqualTo(AppointmentStatus.CANCELLED);
        verify(notificationService).notifyAppointmentEvent(appointment, NotificationType.APPOINTMENT_CANCELLED);
    }

    @Test
    void cancelAppointmentThrowsWhenNotFound() {
        when(appointmentRepository.findById(999L)).thenReturn(Optional.empty());

        assertThatThrownBy(() -> service.cancelAppointment(999L)).isInstanceOf(AppointmentNotFoundException.class);
        verifyNoInteractions(notificationService);
    }

    // --- updateAppointment -------------------------------------------------------

    @Test
    void updateAppointmentAcceptedNotifiesAccepted() {
        Appointment appointment = existingAppointment(6L, AppointmentStatus.SCHEDULED);
        when(appointmentRepository.findById(6L)).thenReturn(Optional.of(appointment));
        UpdateAppointmentRequest request = new UpdateAppointmentRequest();
        request.setStatus(AppointmentStatus.ACCEPTED);

        service.updateAppointment(6L, request);

        verify(notificationService).notifyAppointmentEvent(appointment, NotificationType.APPOINTMENT_ACCEPTED);
    }

    @Test
    void updateAppointmentDeclinedReleasesSlotAndNotifiesDeclined() {
        Appointment appointment = existingAppointment(7L, AppointmentStatus.SCHEDULED);
        when(appointmentRepository.findById(7L)).thenReturn(Optional.of(appointment));
        UpdateAppointmentRequest request = new UpdateAppointmentRequest();
        request.setStatus(AppointmentStatus.DECLINED);

        service.updateAppointment(7L, request);

        verify(slotReservationService).releaseForAppointment(7L);
        verify(notificationService).notifyAppointmentEvent(appointment, NotificationType.APPOINTMENT_DECLINED);
    }

    @Test
    void updateAppointmentWithNoStatusDefaultsToUpdatedAndNotifies() {
        Appointment appointment = existingAppointment(8L, AppointmentStatus.SCHEDULED);
        when(appointmentRepository.findById(8L)).thenReturn(Optional.of(appointment));
        UpdateAppointmentRequest request = new UpdateAppointmentRequest();

        service.updateAppointment(8L, request);

        assertThat(appointment.getStatus()).isEqualTo(AppointmentStatus.UPDATED);
        verify(notificationService).notifyAppointmentEvent(appointment, NotificationType.APPOINTMENT_UPDATED);
    }

    // --- deleteAppointment -------------------------------------------------------

    @Test
    void deleteAppointmentNotifiesDeleted() {
        Appointment appointment = existingAppointment(9L, AppointmentStatus.SCHEDULED);
        when(appointmentRepository.findById(9L)).thenReturn(Optional.of(appointment));

        service.deleteAppointment(9L);

        verify(appointmentRepository).delete(appointment);
        verify(notificationService).notifyAppointmentEvent(appointment, NotificationType.APPOINTMENT_DELETED);
    }

    // --- acceptAppointment -------------------------------------------------------

    @Test
    void acceptAppointmentNotifiesAccepted() {
        Appointment appointment = existingAppointment(10L, AppointmentStatus.SCHEDULED);
        when(appointmentRepository.findById(10L)).thenReturn(Optional.of(appointment));
        AcceptAppointmentRequest request = new AcceptAppointmentRequest();
        request.setAppointmentId(10L);
        request.setStatus(AppointmentStatus.ACCEPTED);

        service.acceptAppointment(request);

        assertThat(appointment.getStatus()).isEqualTo(AppointmentStatus.ACCEPTED);
        verify(notificationService).notifyAppointmentEvent(appointment, NotificationType.APPOINTMENT_ACCEPTED);
    }
}
