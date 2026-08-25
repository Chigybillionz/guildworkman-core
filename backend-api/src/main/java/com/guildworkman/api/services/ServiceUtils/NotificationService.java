package com.guildworkman.api.services.ServiceUtils;

import com.guildworkman.api.data.constants.NotificationType;
import com.guildworkman.api.data.models.Appointment;
import com.guildworkman.api.dto.responses.NotificationResponse;
import org.springframework.data.domain.Page;
import org.springframework.data.domain.Pageable;

import java.util.List;

public interface NotificationService {

    /**
     * Persists a notification for whichever of the appointment's client/worker
     * the given lifecycle event concerns, and schedules the corresponding
     * fan-out email for each. Never rolls back or blocks the caller: the email
     * send happens after the enclosing transaction commits (if any) on a
     * separate thread, and a failed send only marks that notification's email
     * status — it never propagates back here.
     */
    void notifyAppointmentEvent(Appointment appointment, NotificationType type);

    Page<NotificationResponse> getNotifications(String recipientEmail, Pageable pageable);

    long countUnread(String recipientEmail);

    /** @throws com.guildworkman.api.exceptions.NotificationNotFoundException if it doesn't exist or isn't the caller's */
    NotificationResponse markAsRead(Long notificationId, String recipientEmail);

    List<NotificationResponse> markAllAsRead(String recipientEmail);
}
