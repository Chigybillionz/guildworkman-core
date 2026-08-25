package com.guildworkman.api.data.models;

import com.guildworkman.api.data.constants.NotificationEmailStatus;
import com.guildworkman.api.data.constants.NotificationType;
import com.guildworkman.api.data.constants.Role;
import jakarta.persistence.Column;
import jakarta.persistence.Entity;
import jakarta.persistence.EnumType;
import jakarta.persistence.Enumerated;
import jakarta.persistence.GeneratedValue;
import jakarta.persistence.GenerationType;
import jakarta.persistence.Id;
import jakarta.persistence.Index;
import jakarta.persistence.Table;
import lombok.Getter;
import lombok.Setter;

import java.time.Instant;

/**
 * A persisted, readable in-app notification produced by an appointment
 * lifecycle transition (booked/accepted/declined/updated/cancelled/deleted).
 *
 * <p>{@code recipientEmail} is the scoping key for "a user's own
 * notifications": it is the one identifier shared across {@link UserAccount},
 * {@link Client} and {@link SkilledWorker} in this codebase (they are not yet
 * linked by a foreign key — see {@link UserAccount}'s class Javadoc), and it
 * is what {@code JwtAuthenticationFilter} puts in the security principal.
 *
 * <p>The primary key column keeps its original name ({@code notification_id})
 * even though every other field here is new: this table already exists (as
 * an unused stub), and this codebase manages schema via
 * {@code spring.jpa.hibernate.ddl-auto=update}, which only ever adds missing
 * columns — it does not rename or drop them. Renaming the identity column
 * would leave the old primary key behind and could fail to apply cleanly on
 * an already-deployed database. See {@code docs/NOTIFICATION_SERVICE.md}.
 *
 * <p>Deliberately no {@code @Version}: the only mutation after insert is
 * {@code read} flipping {@code false → true} (plus {@code emailStatus}, set
 * exactly once by {@code NotificationEmailDispatcher}), and every writer
 * scopes by {@code recipientEmail} first — see
 * {@code NotificationServiceImpl#markAsRead}/{@code #markAllAsRead}. Two
 * concurrent mark-read calls on the same row (a double-tap, or list + mark-all
 * racing) both want the same end state, so optimistic locking would only add
 * a spurious {@code OptimisticLockException} on an outcome that was already
 * correct, not prevent a lost update.
 */
@Entity
@Table(name = "notifications", indexes = {
        @Index(name = "idx_notifications_recipient_created", columnList = "recipient_email,created_at"),
        @Index(name = "idx_notifications_recipient_unread", columnList = "recipient_email,is_read")
})
@Getter
@Setter
public class Notification {

    @Id
    @GeneratedValue(strategy = GenerationType.IDENTITY)
    private Long notificationId;

    @Column(name = "recipient_email", nullable = false, updatable = false)
    private String recipientEmail;

    @Enumerated(EnumType.STRING)
    @Column(name = "recipient_role", length = 20, updatable = false)
    private Role recipientRole;

    @Enumerated(EnumType.STRING)
    @Column(name = "type", length = 40, updatable = false)
    private NotificationType type;

    @Column(name = "title", length = 200, updatable = false)
    private String title;

    @Column(name = "message", length = 1000, updatable = false)
    private String message;

    /** Not a JPA foreign key on purpose: the appointment may since be deleted. */
    @Column(name = "appointment_id", updatable = false)
    private Long appointmentId;

    @Column(name = "is_read", nullable = false)
    private boolean read;

    @Column(name = "read_at")
    private Instant readAt;

    @Enumerated(EnumType.STRING)
    @Column(name = "email_status", length = 20)
    private NotificationEmailStatus emailStatus = NotificationEmailStatus.PENDING;

    @Column(name = "created_at", updatable = false)
    private Instant createdAt = Instant.now();
}
