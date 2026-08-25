package com.guildworkman.api.services.implmentations;

import com.guildworkman.api.data.constants.NotificationType;
import com.guildworkman.api.data.constants.Role;
import com.guildworkman.api.data.models.Appointment;
import com.guildworkman.api.data.models.Client;
import com.guildworkman.api.data.models.Notification;
import com.guildworkman.api.data.models.SkilledWorker;
import com.guildworkman.api.data.repository.NotificationRepository;
import com.guildworkman.api.dto.responses.NotificationResponse;
import com.guildworkman.api.exceptions.NotificationNotFoundException;
import com.guildworkman.api.services.ServiceUtils.NotificationService;
import lombok.RequiredArgsConstructor;
import org.springframework.data.domain.Page;
import org.springframework.data.domain.Pageable;
import org.springframework.stereotype.Service;
import org.springframework.transaction.support.TransactionSynchronization;
import org.springframework.transaction.support.TransactionSynchronizationManager;

import java.time.Instant;
import java.util.ArrayList;
import java.util.List;

@Service
@RequiredArgsConstructor
public class NotificationServiceImpl implements NotificationService {

    private final NotificationRepository notificationRepository;
    private final NotificationEmailDispatcher emailDispatcher;

    private record Recipient(String email, String name, Role role) {
    }

    @Override
    public void notifyAppointmentEvent(Appointment appointment, NotificationType type) {
        for (Recipient recipient : recipientsFor(appointment, type)) {
            Notification notification = new Notification();
            notification.setRecipientEmail(recipient.email());
            notification.setRecipientRole(recipient.role());
            notification.setType(type);
            notification.setTitle(type.title());
            notification.setMessage(buildMessage(appointment, type, recipient.role()));
            notification.setAppointmentId(appointment.getId());
            notification = notificationRepository.save(notification);

            scheduleEmail(notification, recipient.name());
        }
    }

    @Override
    public Page<NotificationResponse> getNotifications(String recipientEmail, Pageable pageable) {
        return notificationRepository.findByRecipientEmail(recipientEmail, pageable)
                .map(NotificationResponse::from);
    }

    @Override
    public long countUnread(String recipientEmail) {
        return notificationRepository.countByRecipientEmailAndReadFalse(recipientEmail);
    }

    @Override
    public NotificationResponse markAsRead(Long notificationId, String recipientEmail) {
        Notification notification = notificationRepository
                .findByNotificationIdAndRecipientEmail(notificationId, recipientEmail)
                .orElseThrow(() -> new NotificationNotFoundException("Notification not found: " + notificationId));

        if (!notification.isRead()) {
            notification.setRead(true);
            notification.setReadAt(Instant.now());
            notification = notificationRepository.save(notification);
        }
        return NotificationResponse.from(notification);
    }

    @Override
    public List<NotificationResponse> markAllAsRead(String recipientEmail) {
        List<Notification> unread = notificationRepository.findByRecipientEmailAndReadFalse(recipientEmail);
        Instant now = Instant.now();
        List<NotificationResponse> responses = new ArrayList<>(unread.size());
        for (Notification notification : unread) {
            notification.setRead(true);
            notification.setReadAt(now);
            responses.add(NotificationResponse.from(notificationRepository.save(notification)));
        }
        return responses;
    }

    // --- recipients & content -------------------------------------------------

    /**
     * ACCEPTED/DECLINED are the worker's own decision on the client's request,
     * so only the client is told; every other transition affects both sides of
     * the appointment.
     */
    private List<Recipient> recipientsFor(Appointment appointment, NotificationType type) {
        List<Recipient> recipients = new ArrayList<>();
        Client client = appointment.getClient();
        if (client != null && client.getEmail() != null) {
            recipients.add(new Recipient(client.getEmail(), client.getFullName(), Role.CLIENT));
        }

        boolean notifyWorker = type != NotificationType.APPOINTMENT_ACCEPTED
                && type != NotificationType.APPOINTMENT_DECLINED;
        SkilledWorker worker = appointment.getSkilledWorker();
        if (notifyWorker && worker != null && worker.getEmail() != null) {
            recipients.add(new Recipient(worker.getEmail(), worker.getFullName(), Role.SKILLED_WORKER));
        }
        return recipients;
    }

    private String buildMessage(Appointment appointment, NotificationType type, Role recipientRole) {
        String counterpart = recipientRole == Role.CLIENT
                ? nameOrDefault(appointment.getSkilledWorker() != null ? appointment.getSkilledWorker().getFullName() : null, "a worker")
                : nameOrDefault(appointment.getClient() != null ? appointment.getClient().getFullName() : null, "a client");
        String when = appointment.getScheduleTime() != null
                ? appointment.getScheduleTime().toString()
                : "an unspecified time";

        return switch (type) {
            case APPOINTMENT_BOOKED -> recipientRole == Role.CLIENT
                    ? "Your appointment with %s on %s has been booked.".formatted(counterpart, when)
                    : "You have a new appointment request from %s on %s.".formatted(counterpart, when);
            case APPOINTMENT_ACCEPTED -> "Your appointment with %s on %s was accepted.".formatted(counterpart, when);
            case APPOINTMENT_DECLINED -> "Your appointment with %s on %s was declined.".formatted(counterpart, when);
            case APPOINTMENT_UPDATED -> "Your appointment with %s on %s was updated.".formatted(counterpart, when);
            case APPOINTMENT_CANCELLED -> "Your appointment with %s on %s was cancelled.".formatted(counterpart, when);
            case APPOINTMENT_DELETED -> "Your appointment with %s on %s was removed.".formatted(counterpart, when);
        };
    }

    private String nameOrDefault(String name, String fallback) {
        return (name == null || name.isBlank()) ? fallback : name;
    }

    // --- email fan-out ----------------------------------------------------

    /**
     * Defers the actual send until the enclosing transaction (the appointment
     * write that triggered this notification) has committed, so a notification
     * is never emailed for a change that ends up rolled back. If there is no
     * active transaction (e.g. called outside one), the send is scheduled
     * immediately; either way {@link NotificationEmailDispatcher#dispatch} runs
     * asynchronously and never on this thread.
     */
    private void scheduleEmail(Notification notification, String recipientName) {
        Long notificationId = notification.getNotificationId();
        String recipientEmail = notification.getRecipientEmail();
        String subject = notification.getTitle();
        String content = renderEmailHtml(notification.getTitle(), notification.getMessage());

        Runnable send = () -> emailDispatcher.dispatch(notificationId, recipientEmail, recipientName, subject, content);

        if (TransactionSynchronizationManager.isSynchronizationActive()) {
            TransactionSynchronizationManager.registerSynchronization(new TransactionSynchronization() {
                @Override
                public void afterCommit() {
                    send.run();
                }
            });
        } else {
            send.run();
        }
    }

    private String renderEmailHtml(String title, String message) {
        return "<html><body><h2>%s</h2><p>%s</p></body></html>".formatted(title, message);
    }
}
