package com.guildworkman.api.services.implmentations;

import com.guildworkman.api.data.constants.Category;
import com.guildworkman.api.data.constants.NotificationEmailStatus;
import com.guildworkman.api.data.models.Appointment;
import com.guildworkman.api.data.models.Client;
import com.guildworkman.api.data.models.Notification;
import com.guildworkman.api.data.models.SkilledWorker;
import com.guildworkman.api.data.repository.AppointmentRepository;
import com.guildworkman.api.data.repository.ClientRepository;
import com.guildworkman.api.data.repository.NotificationRepository;
import com.guildworkman.api.data.repository.SkilledWorkerRepository;
import com.guildworkman.api.dto.requests.BookAppointmentRequest;
import com.guildworkman.api.services.ServiceUtils.AppointmentService;
import com.guildworkman.api.services.ServiceUtils.MailService;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;
import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.boot.test.context.SpringBootTest;
import org.springframework.boot.test.mock.mockito.MockBean;

import java.time.Duration;
import java.time.Instant;
import java.time.LocalDateTime;
import java.util.List;
import java.util.UUID;

import static org.assertj.core.api.Assertions.assertThat;
import static org.mockito.ArgumentMatchers.any;
import static org.mockito.Mockito.when;

/**
 * End-to-end proof of the guarantee the issue asks for: a down mail provider
 * must never fail or roll back the appointment operation that triggered the
 * notification. Unlike {@link NotificationEmailDispatcherTest} (which drives
 * the dispatcher directly, synchronously, in isolation), this goes through
 * the real {@link AppointmentService} bean, a real transaction, and the real
 * {@code @Async} executor — the only way to actually exercise
 * {@code NotificationServiceImpl#scheduleEmail}'s after-commit wiring and
 * {@code AsyncConfig}'s executor together.
 */
@SpringBootTest(properties = {
        "booking.expiry-sweep-delay-ms=3600000",
        "chain.events.poll-delay-ms=3600000",
        "escrow.orchestration.submit-poll-delay-ms=3600000",
        "escrow.orchestration.confirm-poll-delay-ms=3600000",
        "escrow.reconciliation.poll-delay-ms=3600000"
})
class NotificationEmailFanOutIntegrationTest {

    @Autowired
    private AppointmentService appointmentService;

    @Autowired
    private AppointmentRepository appointmentRepository;

    @Autowired
    private NotificationRepository notificationRepository;

    @Autowired
    private ClientRepository clientRepository;

    @Autowired
    private SkilledWorkerRepository skilledWorkerRepository;

    @MockBean
    private MailService mailService;

    private Client client;
    private SkilledWorker worker;

    @BeforeEach
    void setUp() {
        client = clientRepository.saveAndFlush(newClient());
        worker = skilledWorkerRepository.saveAndFlush(newWorker());
    }

    private Client newClient() {
        Client c = new Client();
        c.setFullName("Fan-Out Test Client");
        c.setEmail("fanout-client-" + UUID.randomUUID() + "@example.com");
        c.setUsername("fanout-client-" + UUID.randomUUID());
        c.setPhoneNumber(UUID.randomUUID().toString());
        return c;
    }

    private SkilledWorker newWorker() {
        SkilledWorker w = new SkilledWorker();
        w.setFullName("Fan-Out Test Worker");
        w.setEmail("fanout-worker-" + UUID.randomUUID() + "@example.com");
        w.setUsername("fanout-worker-" + UUID.randomUUID());
        w.setPhoneNumber(UUID.randomUUID().toString());
        w.setCategory(Category.PLUMBING);
        return w;
    }

    @Test
    void aFailingMailProviderStillLetsTheBookingCommitAndMarksTheEmailFailed() {
        when(mailService.sendMail(any())).thenThrow(new RuntimeException("mail provider is down"));

        BookAppointmentRequest request = new BookAppointmentRequest();
        request.setClientId(client.getId());
        request.setSkilledWorkerId(worker.getId());
        request.setCategory(Category.PLUMBING);
        request.setScheduleTime(LocalDateTime.now().plusDays(1));

        // The booking call itself must succeed: a down mail provider is not
        // this thread's problem, by the time it even knows about it.
        Appointment appointment = appointmentService.bookAppointment(request);

        assertThat(appointment.getId()).isNotNull();
        assertThat(appointmentRepository.findById(appointment.getId())).isPresent();

        List<Notification> notifications = awaitNotificationsFor(appointment.getId(), 2);

        assertThat(notifications).hasSize(2);
        // Both the client and the worker got a persisted notification even
        // though the email behind each one failed.
        assertThat(notifications)
                .extracting(Notification::getRecipientEmail)
                .containsExactlyInAnyOrder(client.getEmail(), worker.getEmail());
        assertThat(notifications)
                .extracting(Notification::getEmailStatus)
                .containsOnly(NotificationEmailStatus.FAILED);
    }

    /**
     * Email dispatch is {@code @Async} and deferred to after-commit, so the
     * {@code FAILED} status lands on a background thread some (short) time
     * after {@code bookAppointment} returns. No Awaitility on the classpath
     * here, so this polls with a bounded, short timeout instead.
     */
    private List<Notification> awaitNotificationsFor(Long appointmentId, int expectedCount) {
        Instant deadline = Instant.now().plus(Duration.ofSeconds(5));
        List<Notification> notifications;
        do {
            notifications = notificationRepository.findAll().stream()
                    .filter(n -> appointmentId.equals(n.getAppointmentId()))
                    .toList();
            if (notifications.size() >= expectedCount
                    && notifications.stream().allMatch(n -> n.getEmailStatus() != NotificationEmailStatus.PENDING)) {
                return notifications;
            }
            sleepQuietly();
        } while (Instant.now().isBefore(deadline));
        return notifications;
    }

    private void sleepQuietly() {
        try {
            Thread.sleep(50);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
    }
}
