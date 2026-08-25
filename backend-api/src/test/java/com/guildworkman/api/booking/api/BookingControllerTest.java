package com.guildworkman.api.booking.api;

import com.fasterxml.jackson.databind.ObjectMapper;
import com.guildworkman.api.booking.model.SlotReservation;
import com.guildworkman.api.booking.repository.SlotReservationRepository;
import com.guildworkman.api.booking.service.SlotReservationService;
import com.guildworkman.api.data.constants.Category;
import com.guildworkman.api.data.models.Client;
import com.guildworkman.api.data.models.SkilledWorker;
import com.guildworkman.api.data.repository.ClientRepository;
import com.guildworkman.api.data.repository.SkilledWorkerRepository;
import com.guildworkman.api.handler.ProblemDetails;
import com.guildworkman.api.services.ServiceUtils.MailService;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;
import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.boot.test.autoconfigure.web.servlet.AutoConfigureMockMvc;
import org.springframework.boot.test.context.SpringBootTest;
import org.springframework.boot.test.mock.mockito.MockBean;
import org.springframework.http.MediaType;
import org.springframework.test.web.servlet.MockMvc;

import java.time.Instant;
import java.time.LocalDateTime;
import java.time.temporal.ChronoUnit;
import java.util.UUID;

import static org.assertj.core.api.Assertions.assertThat;
import static org.springframework.test.web.servlet.request.MockMvcRequestBuilders.delete;
import static org.springframework.test.web.servlet.request.MockMvcRequestBuilders.get;
import static org.springframework.test.web.servlet.request.MockMvcRequestBuilders.post;
import static org.springframework.test.web.servlet.result.MockMvcResultMatchers.content;
import static org.springframework.test.web.servlet.result.MockMvcResultMatchers.header;
import static org.springframework.test.web.servlet.result.MockMvcResultMatchers.jsonPath;
import static org.springframework.test.web.servlet.result.MockMvcResultMatchers.status;

/**
 * HTTP-level contract for the booking endpoints: status codes, the
 * idempotent-replay header, and that every failure — including losing a booking
 * race — comes back as RFC 7807 problem JSON like the rest of the API.
 */
@SpringBootTest(properties = {
        "booking.expiry-sweep-delay-ms=3600000",
        "chain.events.poll-delay-ms=3600000",
        "escrow.orchestration.submit-poll-delay-ms=3600000",
        "escrow.orchestration.confirm-poll-delay-ms=3600000",
        "escrow.reconciliation.poll-delay-ms=3600000"
})
@AutoConfigureMockMvc
class BookingControllerTest {

    @Autowired
    private MockMvc mockMvc;

    @Autowired
    private ObjectMapper objectMapper;

    @Autowired
    private SlotReservationRepository reservations;

    @Autowired
    private SlotReservationService slotReservationService;

    @Autowired
    private ClientRepository clients;

    @Autowired
    private SkilledWorkerRepository skilledWorkers;

    // Confirming a reservation now fans out a notification email; mocked so
    // this never makes a real call to the mail provider.
    @MockBean
    private MailService mailService;

    private SkilledWorker worker;
    private Client client;
    private LocalDateTime slotStart;

    @BeforeEach
    void setUp() {
        reservations.deleteAll();

        SkilledWorker w = new SkilledWorker();
        w.setFullName("Booking Api Worker");
        w.setEmail("worker-" + UUID.randomUUID() + "@example.com");
        w.setUsername("worker-" + UUID.randomUUID());
        w.setPhoneNumber(UUID.randomUUID().toString());
        w.setCategory(Category.CARPENTRY);
        worker = skilledWorkers.saveAndFlush(w);

        Client c = new Client();
        c.setFullName("Booking Api Client");
        c.setEmail("client-" + UUID.randomUUID() + "@example.com");
        c.setUsername("client-" + UUID.randomUUID());
        c.setPhoneNumber(UUID.randomUUID().toString());
        client = clients.saveAndFlush(c);

        slotStart = LocalDateTime.now().plusDays(2).truncatedTo(ChronoUnit.HOURS);
    }

    private String reserveBody(String idempotencyKey, LocalDateTime start) throws Exception {
        return objectMapper.writeValueAsString(
                new ReserveSlotRequest(idempotencyKey, worker.getId(), client.getId(), start, 60));
    }

    @Test
    void reservingASlotReturnsCreatedAndTheHold() throws Exception {
        mockMvc.perform(post("/api/v1/booking/reservations")
                        .contentType(MediaType.APPLICATION_JSON)
                        .content(reserveBody("idem-" + UUID.randomUUID(), slotStart)))
                .andExpect(status().isCreated())
                .andExpect(header().string(BookingController.IDEMPOTENT_REPLAY_HEADER, "false"))
                .andExpect(jsonPath("$.status").value("HELD"))
                .andExpect(jsonPath("$.skilledWorkerId").value(worker.getId()))
                .andExpect(jsonPath("$.expiresAt").exists())
                .andExpect(jsonPath("$.appointmentId").doesNotExist());
    }

    @Test
    void resendingTheSameKeyIsFlaggedAsAReplay() throws Exception {
        String body = reserveBody("idem-" + UUID.randomUUID(), slotStart);

        mockMvc.perform(post("/api/v1/booking/reservations")
                        .contentType(MediaType.APPLICATION_JSON).content(body))
                .andExpect(status().isCreated())
                .andExpect(header().string(BookingController.IDEMPOTENT_REPLAY_HEADER, "false"));

        mockMvc.perform(post("/api/v1/booking/reservations")
                        .contentType(MediaType.APPLICATION_JSON).content(body))
                .andExpect(status().isCreated())
                .andExpect(header().string(BookingController.IDEMPOTENT_REPLAY_HEADER, "true"));

        assertThat(reservations.count()).isEqualTo(1);
    }

    @Test
    void losingTheRaceForASlotIsAConflictProblem() throws Exception {
        mockMvc.perform(post("/api/v1/booking/reservations")
                        .contentType(MediaType.APPLICATION_JSON)
                        .content(reserveBody("idem-" + UUID.randomUUID(), slotStart)))
                .andExpect(status().isCreated());

        mockMvc.perform(post("/api/v1/booking/reservations")
                        .contentType(MediaType.APPLICATION_JSON)
                        .content(reserveBody("idem-" + UUID.randomUUID(), slotStart)))
                .andExpect(status().isConflict())
                .andExpect(content().contentTypeCompatibleWith(ProblemDetails.CONTENT_TYPE))
                .andExpect(jsonPath("$.title").value("Slot unavailable"))
                .andExpect(jsonPath("$.type").value(org.hamcrest.Matchers.endsWith("slot-unavailable")));
    }

    @Test
    void aSlotInThePastFailsValidation() throws Exception {
        mockMvc.perform(post("/api/v1/booking/reservations")
                        .contentType(MediaType.APPLICATION_JSON)
                        .content(reserveBody("idem-" + UUID.randomUUID(), LocalDateTime.now().minusDays(1))))
                .andExpect(status().isBadRequest())
                .andExpect(content().contentTypeCompatibleWith(ProblemDetails.CONTENT_TYPE))
                .andExpect(jsonPath("$.title").value("Validation failed"))
                .andExpect(jsonPath("$.errors.slotStart").exists());
    }

    @Test
    void anEmptyReservationBodyReportsEveryMissingField() throws Exception {
        mockMvc.perform(post("/api/v1/booking/reservations")
                        .contentType(MediaType.APPLICATION_JSON).content("{}"))
                .andExpect(status().isBadRequest())
                .andExpect(jsonPath("$.errors.idempotencyKey").exists())
                .andExpect(jsonPath("$.errors.skilledWorkerId").exists())
                .andExpect(jsonPath("$.errors.clientId").exists())
                .andExpect(jsonPath("$.errors.slotStart").exists());
    }

    @Test
    void confirmingAHoldBooksTheAppointment() throws Exception {
        SlotReservation held = reserve();

        mockMvc.perform(post("/api/v1/booking/reservations/" + held.getId() + "/confirm")
                        .contentType(MediaType.APPLICATION_JSON)
                        .content(objectMapper.writeValueAsString(
                                new ConfirmReservationRequest(Category.CARPENTRY, null))))
                .andExpect(status().isOk())
                .andExpect(jsonPath("$.status").value("CONFIRMED"))
                .andExpect(jsonPath("$.appointmentId").isNumber());
    }

    @Test
    void confirmingWithNoBodyUsesTheReservationsOwnDetails() throws Exception {
        SlotReservation held = reserve();

        mockMvc.perform(post("/api/v1/booking/reservations/" + held.getId() + "/confirm"))
                .andExpect(status().isOk())
                .andExpect(jsonPath("$.status").value("CONFIRMED"));
    }

    @Test
    void confirmingALapsedHoldIsAConflictProblem() throws Exception {
        SlotReservation held = reserve();
        held.setExpiresAt(Instant.now().minusSeconds(60));
        reservations.saveAndFlush(held);

        mockMvc.perform(post("/api/v1/booking/reservations/" + held.getId() + "/confirm"))
                .andExpect(status().isConflict())
                .andExpect(content().contentTypeCompatibleWith(ProblemDetails.CONTENT_TYPE))
                .andExpect(jsonPath("$.title").value("Slot reservation is no longer held"));
    }

    @Test
    void confirmingAnUnknownReservationIsANotFoundProblem() throws Exception {
        mockMvc.perform(post("/api/v1/booking/reservations/9999999/confirm"))
                .andExpect(status().isNotFound())
                .andExpect(content().contentTypeCompatibleWith(ProblemDetails.CONTENT_TYPE))
                .andExpect(jsonPath("$.title").value("Slot reservation not found"));
    }

    @Test
    void releasingAHoldReturnsItInItsReleasedState() throws Exception {
        SlotReservation held = reserve();

        mockMvc.perform(delete("/api/v1/booking/reservations/" + held.getId()))
                .andExpect(status().isOk())
                .andExpect(jsonPath("$.status").value("RELEASED"));
    }

    @Test
    void availabilityListsTheWorkersTakenSlots() throws Exception {
        reserve();

        mockMvc.perform(get("/api/v1/booking/workers/" + worker.getId() + "/availability")
                        .param("from", slotStart.minusHours(1).toString())
                        .param("to", slotStart.plusHours(4).toString()))
                .andExpect(status().isOk())
                .andExpect(jsonPath("$.workerId").value(worker.getId()))
                .andExpect(jsonPath("$.slotDurationMinutes").value(60))
                .andExpect(jsonPath("$.taken.length()").value(1))
                .andExpect(jsonPath("$.taken[0].state").value("HELD"))
                .andExpect(jsonPath("$.taken[0].holdExpiresAt").exists());
    }

    @Test
    void availabilityRejectsAWindowThatEndsBeforeItStarts() throws Exception {
        mockMvc.perform(get("/api/v1/booking/workers/" + worker.getId() + "/availability")
                        .param("from", slotStart.toString())
                        .param("to", slotStart.minusHours(1).toString()))
                .andExpect(status().isBadRequest())
                .andExpect(content().contentTypeCompatibleWith(ProblemDetails.CONTENT_TYPE));
    }

    @Test
    void availabilityRequiresTheWindowParameters() throws Exception {
        mockMvc.perform(get("/api/v1/booking/workers/" + worker.getId() + "/availability"))
                .andExpect(status().isBadRequest())
                .andExpect(jsonPath("$.title").value("Missing request parameter"));
    }

    private SlotReservation reserve() {
        return slotReservationService.reserve(new ReserveSlotRequest(
                "idem-" + UUID.randomUUID(), worker.getId(), client.getId(), slotStart, 60)).reservation();
    }
}
