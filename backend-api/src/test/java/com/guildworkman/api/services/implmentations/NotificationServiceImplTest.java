package com.guildworkman.api.services.implmentations;

import com.guildworkman.api.data.constants.AppointmentStatus;
import com.guildworkman.api.data.constants.Category;
import com.guildworkman.api.data.constants.NotificationType;
import com.guildworkman.api.data.constants.Role;
import com.guildworkman.api.data.models.Appointment;
import com.guildworkman.api.data.models.Client;
import com.guildworkman.api.data.models.Notification;
import com.guildworkman.api.data.models.SkilledWorker;
import com.guildworkman.api.data.repository.NotificationRepository;
import com.guildworkman.api.dto.responses.NotificationResponse;
import com.guildworkman.api.exceptions.NotificationNotFoundException;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;
import org.mockito.ArgumentCaptor;
import org.springframework.data.domain.Page;
import org.springframework.data.domain.PageImpl;
import org.springframework.data.domain.PageRequest;
import org.springframework.data.domain.Pageable;

import java.time.LocalDateTime;
import java.util.List;
import java.util.Optional;

import static org.assertj.core.api.Assertions.assertThat;
import static org.assertj.core.api.Assertions.assertThatThrownBy;
import static org.mockito.ArgumentMatchers.any;
import static org.mockito.ArgumentMatchers.anyLong;
import static org.mockito.ArgumentMatchers.anyString;
import static org.mockito.ArgumentMatchers.eq;
import static org.mockito.Mockito.mock;
import static org.mockito.Mockito.never;
import static org.mockito.Mockito.times;
import static org.mockito.Mockito.verify;
import static org.mockito.Mockito.when;

class NotificationServiceImplTest {

    private NotificationRepository notificationRepository;
    private NotificationEmailDispatcher emailDispatcher;
    private NotificationServiceImpl service;

    @BeforeEach
    void setUp() {
        notificationRepository = mock(NotificationRepository.class);
        emailDispatcher = mock(NotificationEmailDispatcher.class);
        service = new NotificationServiceImpl(notificationRepository, emailDispatcher);

        // save() just returns whatever was passed in, with an id assigned, like a
        // real JpaRepository would for a new row.
        when(notificationRepository.save(any(Notification.class))).thenAnswer(invocation -> {
            Notification notification = invocation.getArgument(0);
            if (notification.getNotificationId() == null) {
                notification.setNotificationId((long) (Math.random() * 1_000_000));
            }
            return notification;
        });
    }

    private static Client client() {
        Client client = new Client();
        client.setId(1L);
        client.setEmail("client@test.com");
        client.setFullName("Cara Client");
        return client;
    }

    private static SkilledWorker worker() {
        SkilledWorker worker = new SkilledWorker();
        worker.setId(2L);
        worker.setEmail("worker@test.com");
        worker.setFullName("Wes Worker");
        return worker;
    }

    private static Appointment appointment(Client client, SkilledWorker worker) {
        Appointment appointment = new Appointment();
        appointment.setId(42L);
        appointment.setStatus(AppointmentStatus.SCHEDULED);
        appointment.setScheduleTime(LocalDateTime.now().plusDays(1));
        appointment.setCategory(Category.ELECTRICAL);
        appointment.setClient(client);
        appointment.setSkilledWorker(worker);
        return appointment;
    }

    // --- notifyAppointmentEvent: recipients -------------------------------------

    @Test
    void bookedNotifiesBothClientAndWorker() {
        Appointment appointment = appointment(client(), worker());

        service.notifyAppointmentEvent(appointment, NotificationType.APPOINTMENT_BOOKED);

        ArgumentCaptor<Notification> captor = ArgumentCaptor.forClass(Notification.class);
        verify(notificationRepository, times(2)).save(captor.capture());
        List<Notification> saved = captor.getAllValues();
        assertThat(saved).extracting(Notification::getRecipientEmail)
                .containsExactlyInAnyOrder("client@test.com", "worker@test.com");
        assertThat(saved).allMatch(n -> n.getType() == NotificationType.APPOINTMENT_BOOKED);
        assertThat(saved).allMatch(n -> n.getAppointmentId().equals(42L));
        assertThat(saved).allMatch(n -> !n.isRead());
    }

    @Test
    void acceptedNotifiesOnlyTheClient() {
        Appointment appointment = appointment(client(), worker());

        service.notifyAppointmentEvent(appointment, NotificationType.APPOINTMENT_ACCEPTED);

        ArgumentCaptor<Notification> captor = ArgumentCaptor.forClass(Notification.class);
        verify(notificationRepository, times(1)).save(captor.capture());
        assertThat(captor.getValue().getRecipientEmail()).isEqualTo("client@test.com");
        assertThat(captor.getValue().getRecipientRole()).isEqualTo(Role.CLIENT);
    }

    @Test
    void declinedNotifiesOnlyTheClient() {
        Appointment appointment = appointment(client(), worker());

        service.notifyAppointmentEvent(appointment, NotificationType.APPOINTMENT_DECLINED);

        verify(notificationRepository, times(1)).save(any());
    }

    @Test
    void missingWorkerOnlyNotifiesTheClient() {
        Appointment appointment = appointment(client(), null);

        service.notifyAppointmentEvent(appointment, NotificationType.APPOINTMENT_BOOKED);

        ArgumentCaptor<Notification> captor = ArgumentCaptor.forClass(Notification.class);
        verify(notificationRepository, times(1)).save(captor.capture());
        assertThat(captor.getValue().getRecipientEmail()).isEqualTo("client@test.com");
    }

    @Test
    void everyNotificationSchedulesAFanOutEmail() {
        Appointment appointment = appointment(client(), worker());

        service.notifyAppointmentEvent(appointment, NotificationType.APPOINTMENT_UPDATED);

        // No transaction is active in this unit test, so the email dispatch runs
        // immediately rather than deferring to an afterCommit callback.
        verify(emailDispatcher).dispatch(anyLong(), eq("client@test.com"), eq("Cara Client"), anyString(), anyString());
        verify(emailDispatcher).dispatch(anyLong(), eq("worker@test.com"), eq("Wes Worker"), anyString(), anyString());
    }

    // --- pagination / read state -------------------------------------------------

    @Test
    void getNotificationsDelegatesToRepositoryAndMapsToResponses() {
        Notification notification = new Notification();
        notification.setNotificationId(1L);
        notification.setRecipientEmail("client@test.com");
        notification.setType(NotificationType.APPOINTMENT_BOOKED);
        notification.setTitle("Appointment booked");
        notification.setMessage("hi");
        Pageable pageable = PageRequest.of(0, 20);
        Page<Notification> page = new PageImpl<>(List.of(notification), pageable, 1);
        when(notificationRepository.findByRecipientEmail("client@test.com", pageable)).thenReturn(page);

        Page<NotificationResponse> result = service.getNotifications("client@test.com", pageable);

        assertThat(result.getTotalElements()).isEqualTo(1);
        assertThat(result.getContent().get(0).id()).isEqualTo(1L);
    }

    @Test
    void countUnreadDelegatesToRepository() {
        when(notificationRepository.countByRecipientEmailAndReadFalse("client@test.com")).thenReturn(3L);

        assertThat(service.countUnread("client@test.com")).isEqualTo(3L);
    }

    // --- ownership scoping --------------------------------------------------------

    @Test
    void markAsReadOnlyFindsTheCallersOwnNotification() {
        Notification notification = new Notification();
        notification.setNotificationId(1L);
        notification.setRecipientEmail("client@test.com");
        when(notificationRepository.findByNotificationIdAndRecipientEmail(1L, "client@test.com"))
                .thenReturn(Optional.of(notification));

        NotificationResponse response = service.markAsRead(1L, "client@test.com");

        assertThat(response.read()).isTrue();
        assertThat(notification.isRead()).isTrue();
        assertThat(notification.getReadAt()).isNotNull();
    }

    @Test
    void markAsReadThrowsForANotificationThatIsntTheCallers() {
        // The repository query is scoped by recipientEmail, so a notification id
        // that belongs to someone else simply doesn't match — same 404 as one
        // that doesn't exist at all, so a caller can't distinguish "not mine"
        // from "never existed".
        when(notificationRepository.findByNotificationIdAndRecipientEmail(1L, "someone-else@test.com"))
                .thenReturn(Optional.empty());

        assertThatThrownBy(() -> service.markAsRead(1L, "someone-else@test.com"))
                .isInstanceOf(NotificationNotFoundException.class);
    }

    @Test
    void markAsReadIsIdempotentForAnAlreadyReadNotification() {
        Notification notification = new Notification();
        notification.setNotificationId(1L);
        notification.setRecipientEmail("client@test.com");
        notification.setRead(true);
        LocalDateTime firstReadAt = LocalDateTime.now().minusDays(1);
        when(notificationRepository.findByNotificationIdAndRecipientEmail(1L, "client@test.com"))
                .thenReturn(Optional.of(notification));

        service.markAsRead(1L, "client@test.com");

        // Already read: no redundant save.
        verify(notificationRepository, never()).save(any());
    }

    @Test
    void markAllAsReadOnlyTouchesTheCallersUnreadNotifications() {
        Notification first = new Notification();
        first.setNotificationId(1L);
        first.setRecipientEmail("client@test.com");
        Notification second = new Notification();
        second.setNotificationId(2L);
        second.setRecipientEmail("client@test.com");
        when(notificationRepository.findByRecipientEmailAndReadFalse("client@test.com"))
                .thenReturn(List.of(first, second));

        List<NotificationResponse> result = service.markAllAsRead("client@test.com");

        assertThat(result).hasSize(2);
        assertThat(result).allMatch(NotificationResponse::read);
        verify(notificationRepository, times(2)).save(any());
    }
}
