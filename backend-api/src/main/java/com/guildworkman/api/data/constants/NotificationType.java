package com.guildworkman.api.data.constants;

/**
 * The appointment lifecycle transitions that produce a notification. Each
 * value carries the title used for both the in-app notification and the
 * fan-out email's subject line.
 */
public enum NotificationType {
    APPOINTMENT_BOOKED("Appointment booked"),
    APPOINTMENT_ACCEPTED("Appointment accepted"),
    APPOINTMENT_DECLINED("Appointment declined"),
    APPOINTMENT_UPDATED("Appointment updated"),
    APPOINTMENT_CANCELLED("Appointment cancelled"),
    APPOINTMENT_DELETED("Appointment removed");

    private final String title;

    NotificationType(String title) {
        this.title = title;
    }

    public String title() {
        return title;
    }

    /**
     * Maps the status an appointment was just saved with to the notification it
     * produces. Used by {@code updateAppointment}, which applies whatever status
     * the caller asked for (ACCEPTED/DECLINED/CANCELLED/UPDATED) through a
     * single endpoint.
     */
    public static NotificationType fromAppointmentStatus(AppointmentStatus status) {
        return switch (status) {
            case ACCEPTED -> APPOINTMENT_ACCEPTED;
            case DECLINED -> APPOINTMENT_DECLINED;
            case CANCELLED -> APPOINTMENT_CANCELLED;
            case SCHEDULED -> APPOINTMENT_BOOKED;
            case UPDATED -> APPOINTMENT_UPDATED;
        };
    }
}
