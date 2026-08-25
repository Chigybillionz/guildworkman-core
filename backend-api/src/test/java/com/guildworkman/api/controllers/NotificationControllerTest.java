package com.guildworkman.api.controllers;

import com.guildworkman.api.data.constants.NotificationType;
import com.guildworkman.api.data.constants.Role;
import com.guildworkman.api.data.models.Notification;
import com.guildworkman.api.data.models.UserAccount;
import com.guildworkman.api.data.repository.NotificationRepository;
import com.guildworkman.api.handler.ProblemDetails;
import com.guildworkman.api.security.JwtService;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;
import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.boot.test.autoconfigure.web.servlet.AutoConfigureMockMvc;
import org.springframework.boot.test.context.SpringBootTest;
import org.springframework.test.web.servlet.MockMvc;

import static org.assertj.core.api.Assertions.assertThat;
import static org.springframework.test.web.servlet.request.MockMvcRequestBuilders.get;
import static org.springframework.test.web.servlet.request.MockMvcRequestBuilders.put;
import static org.springframework.test.web.servlet.result.MockMvcResultMatchers.content;
import static org.springframework.test.web.servlet.result.MockMvcResultMatchers.jsonPath;
import static org.springframework.test.web.servlet.result.MockMvcResultMatchers.status;

/**
 * HTTP-level coverage for the ownership guarantee: a caller can list, count,
 * and mark-read only their own notifications, scoped by the email in their
 * JWT — never by an id or query parameter a caller could substitute for
 * someone else's.
 */
@SpringBootTest(properties = {
        "booking.expiry-sweep-delay-ms=3600000",
        "chain.events.poll-delay-ms=3600000",
        "escrow.orchestration.submit-poll-delay-ms=3600000",
        "escrow.orchestration.confirm-poll-delay-ms=3600000",
        "escrow.reconciliation.poll-delay-ms=3600000"
})
@AutoConfigureMockMvc
class NotificationControllerTest {

    @Autowired
    private MockMvc mockMvc;

    @Autowired
    private NotificationRepository notificationRepository;

    @Autowired
    private JwtService jwtService;

    private String clientToken;

    @BeforeEach
    void setUp() {
        notificationRepository.deleteAll();
        clientToken = tokenFor("client@test.com");
    }

    private String tokenFor(String email) {
        UserAccount account = new UserAccount();
        account.setId(1L);
        account.setEmail(email);
        account.setRole(Role.CLIENT);
        return jwtService.generateAccessToken(account);
    }

    private Notification seed(String recipientEmail, boolean read) {
        Notification notification = new Notification();
        notification.setRecipientEmail(recipientEmail);
        notification.setRecipientRole(Role.CLIENT);
        notification.setType(NotificationType.APPOINTMENT_BOOKED);
        notification.setTitle("Appointment booked");
        notification.setMessage("Your appointment was booked.");
        notification.setRead(read);
        return notificationRepository.save(notification);
    }

    @Test
    void listingWithoutATokenIsUnauthorized() throws Exception {
        mockMvc.perform(get("/api/v1/notifications"))
                .andExpect(status().isUnauthorized())
                .andExpect(content().contentTypeCompatibleWith(ProblemDetails.CONTENT_TYPE));
    }

    @Test
    void listReturnsOnlyTheCallersOwnNotifications() throws Exception {
        seed("client@test.com", false);
        seed("someone-else@test.com", false);

        mockMvc.perform(get("/api/v1/notifications")
                        .header("Authorization", "Bearer " + clientToken))
                .andExpect(status().isOk())
                .andExpect(jsonPath("$.content.length()").value(1))
                .andExpect(jsonPath("$.content[0].title").value("Appointment booked"))
                .andExpect(jsonPath("$.page.totalElements").value(1));
    }

    @Test
    void unreadCountCountsOnlyTheCallersUnread() throws Exception {
        seed("client@test.com", false);
        seed("client@test.com", true);
        seed("someone-else@test.com", false);

        mockMvc.perform(get("/api/v1/notifications/unread-count")
                        .header("Authorization", "Bearer " + clientToken))
                .andExpect(status().isOk())
                .andExpect(jsonPath("$.unread").value(1));
    }

    @Test
    void markAsReadOnAnotherUsersNotificationIsNotFound() throws Exception {
        Notification notification = seed("someone-else@test.com", false);

        mockMvc.perform(put("/api/v1/notifications/" + notification.getNotificationId() + "/read")
                        .header("Authorization", "Bearer " + clientToken))
                .andExpect(status().isNotFound())
                .andExpect(content().contentTypeCompatibleWith(ProblemDetails.CONTENT_TYPE));
    }

    @Test
    void markAsReadOnOwnNotificationSucceeds() throws Exception {
        Notification notification = seed("client@test.com", false);

        mockMvc.perform(put("/api/v1/notifications/" + notification.getNotificationId() + "/read")
                        .header("Authorization", "Bearer " + clientToken))
                .andExpect(status().isOk())
                .andExpect(jsonPath("$.read").value(true));

        assertThat(notificationRepository.findById(notification.getNotificationId()).orElseThrow().isRead()).isTrue();
    }

    @Test
    void markAllAsReadOnlyTouchesTheCallersOwnNotifications() throws Exception {
        seed("client@test.com", false);
        seed("client@test.com", false);
        seed("someone-else@test.com", false);

        mockMvc.perform(put("/api/v1/notifications/read-all")
                        .header("Authorization", "Bearer " + clientToken))
                .andExpect(status().isOk())
                .andExpect(jsonPath("$.length()").value(2));

        assertThat(notificationRepository.countByRecipientEmailAndReadFalse("someone-else@test.com")).isEqualTo(1);
        assertThat(notificationRepository.countByRecipientEmailAndReadFalse("client@test.com")).isEqualTo(0);
    }
}
