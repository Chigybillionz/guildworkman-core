package com.guildworkman.api.controllers;

import com.guildworkman.api.dto.responses.NotificationResponse;
import com.guildworkman.api.dto.responses.UnreadCountResponse;
import com.guildworkman.api.services.ServiceUtils.NotificationService;
import io.swagger.v3.oas.annotations.Operation;
import io.swagger.v3.oas.annotations.security.SecurityRequirement;
import lombok.RequiredArgsConstructor;
import org.springframework.data.domain.Pageable;
import org.springframework.data.domain.Sort;
import org.springframework.data.web.PageableDefault;
import org.springframework.data.web.PagedModel;
import org.springframework.security.core.annotation.AuthenticationPrincipal;
import org.springframework.web.bind.annotation.GetMapping;
import org.springframework.web.bind.annotation.PathVariable;
import org.springframework.web.bind.annotation.PutMapping;
import org.springframework.web.bind.annotation.RequestMapping;
import org.springframework.web.bind.annotation.RestController;

import java.util.List;

/**
 * A user's own in-app notifications. Every endpoint scopes to the calling
 * user's email (the JWT principal — see {@code JwtAuthenticationFilter}), so
 * there is no id-based lookup that could leak another user's notifications;
 * {@code markAsRead} 404s on a notification id that exists but belongs to
 * someone else, rather than exposing it.
 */
@RestController
@RequestMapping("/api/v1/notifications")
@RequiredArgsConstructor
@SecurityRequirement(name = "bearerAuth")
public class NotificationController {

    private final NotificationService notificationService;

    @GetMapping
    @Operation(summary = "List the caller's notifications, newest first", description =
            "Paginated. Default page size 20; override with the standard page/size/sort query params.")
    public PagedModel<NotificationResponse> list(
            @AuthenticationPrincipal String recipientEmail,
            @PageableDefault(size = 20, sort = "createdAt", direction = Sort.Direction.DESC) Pageable pageable) {
        return new PagedModel<>(notificationService.getNotifications(recipientEmail, pageable));
    }

    @GetMapping("/unread-count")
    @Operation(summary = "Count of the caller's unread notifications")
    public UnreadCountResponse unreadCount(@AuthenticationPrincipal String recipientEmail) {
        return new UnreadCountResponse(notificationService.countUnread(recipientEmail));
    }

    @PutMapping("/{id}/read")
    @Operation(summary = "Mark one notification read", description =
            "404 if the notification doesn't exist or doesn't belong to the caller.")
    public NotificationResponse markAsRead(@AuthenticationPrincipal String recipientEmail, @PathVariable Long id) {
        return notificationService.markAsRead(id, recipientEmail);
    }

    @PutMapping("/read-all")
    @Operation(summary = "Mark all of the caller's unread notifications read")
    public List<NotificationResponse> markAllAsRead(@AuthenticationPrincipal String recipientEmail) {
        return notificationService.markAllAsRead(recipientEmail);
    }
}
