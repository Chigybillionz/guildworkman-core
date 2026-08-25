package com.guildworkman.api.services.implmentations;

import com.guildworkman.api.data.constants.NotificationEmailStatus;
import com.guildworkman.api.data.models.Notification;
import com.guildworkman.api.data.repository.NotificationRepository;
import com.guildworkman.api.dto.requests.SendMailRequest;
import com.guildworkman.api.services.ServiceUtils.MailService;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;
import org.mockito.ArgumentCaptor;

import java.util.Optional;

import static org.assertj.core.api.Assertions.assertThat;
import static org.assertj.core.api.Assertions.assertThatCode;
import static org.mockito.ArgumentMatchers.any;
import static org.mockito.Mockito.mock;
import static org.mockito.Mockito.verify;
import static org.mockito.Mockito.when;

/**
 * {@link NotificationEmailDispatcher} is the boundary that has to absorb a
 * down or failing mail provider without letting it escape — see
 * {@code NotificationServiceImpl#scheduleEmail}, which runs this after the
 * appointment write it reports on has already committed. Runs {@code dispatch}
 * synchronously (the {@code @Async} annotation only applies through Spring's
 * proxy, not a directly-constructed instance), which is fine here: the
 * behaviour under test is what happens to the notification row, not the
 * threading.
 */
class NotificationEmailDispatcherTest {

    private MailService mailService;
    private NotificationRepository notificationRepository;
    private NotificationEmailDispatcher dispatcher;

    @BeforeEach
    void setUp() {
        mailService = mock(MailService.class);
        notificationRepository = mock(NotificationRepository.class);
        dispatcher = new NotificationEmailDispatcher(mailService, notificationRepository);
    }

    private static Notification notification() {
        Notification notification = new Notification();
        notification.setNotificationId(1L);
        notification.setEmailStatus(NotificationEmailStatus.PENDING);
        return notification;
    }

    @Test
    void aSuccessfulSendMarksTheNotificationSent() {
        Notification notification = notification();
        when(notificationRepository.findById(1L)).thenReturn(Optional.of(notification));
        when(mailService.sendMail(any())).thenReturn("mail sent successfully");

        dispatcher.dispatch(1L, "client@test.com", "Cara Client", "Appointment booked", "<p>hi</p>");

        ArgumentCaptor<SendMailRequest> captor = ArgumentCaptor.forClass(SendMailRequest.class);
        verify(mailService).sendMail(captor.capture());
        assertThat(captor.getValue().getRecipientEmail()).isEqualTo("client@test.com");
        assertThat(captor.getValue().getRecipientName()).isEqualTo("Cara Client");
        assertThat(captor.getValue().getSubject()).isEqualTo("Appointment booked");
        assertThat(notification.getEmailStatus()).isEqualTo(NotificationEmailStatus.SENT);
        verify(notificationRepository).save(notification);
    }

    /** The mail-provider-down case: a thrown exception must never escape. */
    @Test
    void aFailingMailProviderMarksTheNotificationFailedAndDoesNotThrow() {
        Notification notification = notification();
        when(notificationRepository.findById(1L)).thenReturn(Optional.of(notification));
        when(mailService.sendMail(any())).thenThrow(new RuntimeException("mail provider is down"));

        assertThatCode(() -> dispatcher.dispatch(1L, "client@test.com", "Cara Client", "subj", "content"))
                .doesNotThrowAnyException();

        assertThat(notification.getEmailStatus()).isEqualTo(NotificationEmailStatus.FAILED);
        verify(notificationRepository).save(notification);
    }

    @Test
    void aNotificationThatNoLongerExistsIsSilentlySkipped() {
        when(notificationRepository.findById(1L)).thenReturn(Optional.empty());
        when(mailService.sendMail(any())).thenReturn("mail sent successfully");

        assertThatCode(() -> dispatcher.dispatch(1L, "client@test.com", "Cara Client", "subj", "content"))
                .doesNotThrowAnyException();

        verify(notificationRepository, org.mockito.Mockito.never()).save(any());
    }
}
