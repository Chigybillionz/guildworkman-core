package com.guildworkman.api.dto.responses;

import com.guildworkman.api.data.constants.NotificationType;
import com.guildworkman.api.data.models.Notification;

import java.time.Instant;

public record NotificationResponse(
        Long id,
        NotificationType type,
        String title,
        String message,
        Long appointmentId,
        boolean read,
        Instant readAt,
        Instant createdAt
) {
    public static NotificationResponse from(Notification notification) {
        return new NotificationResponse(
                notification.getNotificationId(),
                notification.getType(),
                notification.getTitle(),
                notification.getMessage(),
                notification.getAppointmentId(),
                notification.isRead(),
                notification.getReadAt(),
                notification.getCreatedAt());
    }
}
