package com.guildworkman.api.data.constants;

/**
 * Tracks the fan-out email for a notification, independent of the
 * notification row itself: the in-app notification is always persisted, but
 * the email is sent asynchronously and can fail without affecting it. See
 * {@code NotificationEmailDispatcher}.
 */
public enum NotificationEmailStatus {
    PENDING,
    SENT,
    FAILED
}
