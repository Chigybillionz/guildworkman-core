package com.guildworkman.api.services;

import jakarta.persistence.EntityManager;
import jakarta.persistence.PersistenceContext;
import com.guildworkman.api.data.constants.AppointmentStatus;
import com.guildworkman.api.data.constants.Category;
import com.guildworkman.api.data.models.Client;
import com.guildworkman.api.data.repository.AddressRepository;
import com.guildworkman.api.data.repository.AppointmentRepository;
import com.guildworkman.api.dto.requests.BookAppointmentRequest;
import com.guildworkman.api.dto.requests.RegistrationRequest;
import com.guildworkman.api.dto.requests.UpdateAppointmentRequest;
import com.guildworkman.api.dto.requests.UpdateClientRequest;
import com.guildworkman.api.dto.responses.ClientRegistrationResponse;
import com.guildworkman.api.data.repository.ClientRepository;
import com.guildworkman.api.dto.responses.UpdateClientResponse;
import com.guildworkman.api.dto.responses.UpdateSkilledWorkerResponse;
import com.guildworkman.api.dto.responses.SkilledWorkerRegistrationResponse;
import com.guildworkman.api.dto.responses.ViewAllAppointmentsResponse;
import com.guildworkman.api.services.ServiceUtils.ClientService;
import com.guildworkman.api.services.ServiceUtils.MailService;
import com.guildworkman.api.services.ServiceUtils.SkilledWorkerService;
import org.jetbrains.annotations.NotNull;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;
import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.boot.test.context.SpringBootTest;
import org.springframework.boot.test.mock.mockito.MockBean;
import org.springframework.test.context.jdbc.Sql;
import org.springframework.transaction.annotation.Transactional;

import java.math.BigDecimal;
import java.time.LocalDateTime;
import java.util.List;

import static com.guildworkman.api.data.constants.AppointmentStatus.CANCELLED;
import static org.assertj.core.api.Assertions.assertThat;


@SpringBootTest
@Transactional
//@Sql(scripts = "/db/data.sql")
public class ClientServiceTest {
    @Autowired
    private ClientService clientService;
    @Autowired
    private ClientRepository clientRepository;
    @Autowired
    private AddressRepository addressRepository;
    @Autowired
    private AppointmentRepository appointmentRepository;
    @Autowired
    private SkilledWorkerService skilledWorkerService;
    @PersistenceContext
    private EntityManager entityManager;
    // Booking an appointment now fans out a notification email; mocked so
    // these tests never make a real call to the mail provider.
    @MockBean
    private MailService mailService;

    @BeforeEach
    public void setUp() {
      appointmentRepository.deleteAll();
    }

    /** Push pending changes to the DB and drop the first-level cache, so the next
     *  read reflects what was actually persisted rather than the in-memory graph.
     *  Every test here runs in one transaction, so without this a state change can
     *  appear to work while never reaching the database. */
    private void reloadFromDatabase() {
        entityManager.flush();
        entityManager.clear();
    }

    @Test
    public void registerClient() {
        long before = clientService.getNumberOfUsers();
        RegistrationRequest registerClientRequest = getRegistrationRequest();
        ClientRegistrationResponse response = clientService.registerClient(registerClientRequest);
        assertThat(response).isNotNull();
        assertThat(clientService.getNumberOfUsers()).isEqualTo(before + 1);

    }

    private static @NotNull RegistrationRequest getRegistrationRequest() {
        RegistrationRequest registerClientRequest = new RegistrationRequest();
        registerClientRequest.setFullName("John Doe");
        registerClientRequest.setEmail("john@doe.com");
        registerClientRequest.setPassword("password1");
        return registerClientRequest;
    }

    @Test
    public void updateUserProfileTest() {
        long before = clientService.getNumberOfUsers();
        UpdateClientRequest updateRequest = new UpdateClientRequest();
        RegistrationRequest registerClientRequest = getRegistrationRequest();
        ClientRegistrationResponse response = clientService.registerClient(registerClientRequest);
        assertThat(response).isNotNull();
        assertThat(clientService.getNumberOfUsers()).isEqualTo(before + 1);

        updateRequest.setClientId(response.getClientId());
        updateRequest.setUsername("Jdoe");
        updateRequest.setPassword("password1");
        updateRequest.setHouseNumber("312");
        updateRequest.setStreet("Herbert Macaulay way");
        updateRequest.setArea("Yaba");
        UpdateClientResponse updateResponse = clientService.updateClientProfile(updateRequest);
        assertThat(updateResponse).isNotNull();
        assertThat(clientService.getNumberOfUsers()).isEqualTo(before + 1);
        assertThat(updateResponse.getClientId()).isEqualTo(response.getClientId());

    }

    @Test
    public void viewAllAppointment_returnsEveryAppointment_withIdAndStatus() {
        ClientRegistrationResponse client = clientService.registerClient(getRegistrationRequest());
        Long clientId = client.getClientId();

        clientService.bookAppointment(
                bookRequest(clientId, Category.ELECTRICAL, LocalDateTime.now().plusDays(2)));
        clientService.bookAppointment(
                bookRequest(clientId, Category.PLUMBING, LocalDateTime.now().plusDays(5)));

        List<ViewAllAppointmentsResponse> appointments = clientService.viewAllAppointment(clientId);

        // Previously this returned only appointments.get(0), mapped to two fields.
        assertThat(appointments).hasSize(2);
        assertThat(appointments).allSatisfy(appointment -> {
            // The id is what cancel/update/delete take as ?appointmentId= — without it
            // the whole appointment-management flow is unreachable from a client.
            assertThat(appointment.getId()).isNotNull();
            assertThat(appointment.getStatus()).isEqualTo(AppointmentStatus.SCHEDULED);
            assertThat(appointment.getScheduleTime()).isNotNull();
        });
        assertThat(appointments)
                .extracting(ViewAllAppointmentsResponse::getCategory)
                .containsExactlyInAnyOrder(Category.ELECTRICAL, Category.PLUMBING);
    }

    @Test
    public void bookAppointment_recordsTheBookedWorker_andAmount() {
        ClientRegistrationResponse client = clientService.registerClient(getRegistrationRequest());

        RegistrationRequest workerRequest = new RegistrationRequest();
        workerRequest.setFullName("Chidi Okonkwo");
        workerRequest.setEmail("chidi@sparks.com");
        workerRequest.setPassword("password1");
        SkilledWorkerRegistrationResponse worker =
                skilledWorkerService.registerSkilledWorker(workerRequest);

        BookAppointmentRequest request =
                bookRequest(client.getClientId(), Category.ELECTRICAL, LocalDateTime.now().plusDays(3));
        request.setSkilledWorkerId(worker.getSkilledWorkerId());
        request.setAmount(BigDecimal.valueOf(8800));
        clientService.bookAppointment(request);

        List<ViewAllAppointmentsResponse> appointments =
                clientService.viewAllAppointment(client.getClientId());

        assertThat(appointments).hasSize(1);
        ViewAllAppointmentsResponse appointment = appointments.get(0);

        // The API previously never recorded WHICH worker was booked — the request
        // had no skilledWorkerId and nothing set Appointment.skilledWorker.
        assertThat(appointment.getWorker()).isNotNull();
        assertThat(appointment.getWorker().getId()).isEqualTo(worker.getSkilledWorkerId());
        assertThat(appointment.getWorker().getFullName()).isEqualTo("Chidi Okonkwo");
        assertThat(appointment.getAmount()).isEqualByComparingTo(BigDecimal.valueOf(8800));
    }

    @Test
    public void bookAppointment_withoutAWorker_stillSucceeds() {
        // skilledWorkerId is optional, so existing callers keep working.
        ClientRegistrationResponse client = clientService.registerClient(getRegistrationRequest());
        clientService.bookAppointment(
                bookRequest(client.getClientId(), Category.PLUMBING, LocalDateTime.now().plusDays(1)));

        List<ViewAllAppointmentsResponse> appointments =
                clientService.viewAllAppointment(client.getClientId());

        assertThat(appointments).hasSize(1);
        assertThat(appointments.get(0).getWorker()).isNull();
    }

    @Test
    public void cancelAppointment_marksItCancelled_andKeepsTheRecord() {
        ClientRegistrationResponse client = clientService.registerClient(getRegistrationRequest());
        clientService.bookAppointment(
                bookRequest(client.getClientId(), Category.ELECTRICAL, LocalDateTime.now().plusDays(2)));

        Long appointmentId = clientService.viewAllAppointment(client.getClientId()).get(0).getId();
        clientService.cancelAppointment(appointmentId);
        reloadFromDatabase();

        List<ViewAllAppointmentsResponse> appointments =
                clientService.viewAllAppointment(client.getClientId());

        // Cancelling used to remove the appointment from client.getAppointment(),
        // and because that relation is orphanRemoval = true the row was DELETED —
        // the client lost the record and CANCELLED was never observable.
        assertThat(appointments).hasSize(1);
        assertThat(appointments.get(0).getId()).isEqualTo(appointmentId);
        assertThat(appointments.get(0).getStatus()).isEqualTo(CANCELLED);
    }

    @Test
    public void updateAppointment_appliesTheRequestedStatus() {
        ClientRegistrationResponse client = clientService.registerClient(getRegistrationRequest());
        clientService.bookAppointment(
                bookRequest(client.getClientId(), Category.PLUMBING, LocalDateTime.now().plusDays(2)));

        Long appointmentId = clientService.viewAllAppointment(client.getClientId()).get(0).getId();

        UpdateAppointmentRequest request = new UpdateAppointmentRequest();
        request.setStatus(AppointmentStatus.ACCEPTED);
        clientService.updateAppointment(appointmentId, request);
        reloadFromDatabase();

        // The requested status used to be ignored entirely: the request was mapped
        // into a throwaway Appointment and the status hardcoded to UPDATED, so
        // accept/decline never changed anything.
        assertThat(clientService.viewAllAppointment(client.getClientId()).get(0).getStatus())
                .isEqualTo(AppointmentStatus.ACCEPTED);
    }

    @Test
    public void deleteAppointment_withAWorkerAttached_removesTheRowFromTheDatabase() {
        ClientRegistrationResponse client = clientService.registerClient(getRegistrationRequest());
        SkilledWorkerRegistrationResponse worker = registerWorker("Bola Adeyemi", "bola@wood.com");

        // Attach a worker: that's the realistic case now that bookings record one,
        // and it's the case that broke in a real request. Client.appointment AND
        // SkilledWorker.appointment are both cascade = ALL + orphanRemoval + EAGER,
        // so a worker left holding this appointment cascades a PERSIST back over it
        // at flush and RESURRECTS it — no DELETE is issued and the caller is still
        // told it succeeded.
        //
        // Caveat, stated plainly: this test passes with or without the worker-side
        // detach, because a single-transaction test doesn't reproduce the session
        // state of a real request. The fix was verified against a running server
        // (Hibernate emits `delete from appointments where id=?`, and the row is
        // gone from Postgres). This test guards the delete contract, not that bug.
        BookAppointmentRequest request =
                bookRequest(client.getClientId(), Category.CARPENTRY, LocalDateTime.now().plusDays(2));
        request.setSkilledWorkerId(worker.getSkilledWorkerId());
        clientService.bookAppointment(request);

        // Clear the session BEFORE deleting. Otherwise the worker still carries the
        // empty collection it was registered with, the delete path never hydrates it
        // from the database, and nothing resurrects the appointment — the test would
        // pass against the broken code. A real HTTP request always starts cold.
        reloadFromDatabase();

        Long appointmentId = clientService.viewAllAppointment(client.getClientId()).get(0).getId();
        clientService.deleteAppointment(appointmentId);

        // Flush and clear again, or the assertion only observes the in-memory
        // collection mutation and passes even when no DELETE reaches the database.
        reloadFromDatabase();

        assertThat(appointmentRepository.findById(appointmentId)).isEmpty();
        assertThat(clientService.viewAllAppointment(client.getClientId())).isEmpty();
    }

    private SkilledWorkerRegistrationResponse registerWorker(String fullName, String email) {
        RegistrationRequest request = new RegistrationRequest();
        request.setFullName(fullName);
        request.setEmail(email);
        request.setPassword("password1");
        return skilledWorkerService.registerSkilledWorker(request);
    }

    @Test
    public void viewAllAppointment_forClientWithNoAppointments_returnsEmptyList() {
        ClientRegistrationResponse client = clientService.registerClient(getRegistrationRequest());

        // Previously this threw IllegalArgumentException; an empty list is the correct
        // result for a client who simply hasn't booked anything yet.
        assertThat(clientService.viewAllAppointment(client.getClientId())).isEmpty();
    }

    private static @NotNull BookAppointmentRequest bookRequest(Long clientId,
                                                               Category category,
                                                               LocalDateTime scheduleTime) {
        BookAppointmentRequest request = new BookAppointmentRequest();
        request.setClientId(clientId);
        request.setCategory(category);
        request.setStatus(AppointmentStatus.SCHEDULED);
        request.setScheduleTime(scheduleTime);
        return request;
    }

//    @Test
//    public void testThatClientCan_bookAppointmentTest() {
//        BookAppointmentRequest request = new BookAppointmentRequest();
//        request.setClientId(2L);
//        request.setCategory(Category.ELECTRICAL);
//        request.setStatus(AppointmentStatus.SCHEDULED);
//        request.setScheduleTime(java.time.LocalDateTime.now().plusDays(6));
//        request.setSkilledWorkerId(204L);
//        clientService.bookAppointment(request);
//        assertThat(clientRepository.findById(2L).get()
//                .getAppointment().size()).isEqualTo(1);
//
//    }

//    @Test
//    public void testThatClientCan_cancelAppointmentTest() {
//        BookAppointmentRequest request = new BookAppointmentRequest();
//        request.setClientId(3L);
//        request.setCategory(Category.ELECTRICAL);
//        request.setStatus(AppointmentStatus.SCHEDULED);
//        request.setScheduleTime(java.time.LocalDateTime.now().plusDays(6));
//        request.setSkilledWorkerId(204L);
//        clientService.bookAppointment(request);
//        clientService.cancelAppointment(1L,3L);
//        Client client = clientRepository.findById(3L).orElseThrow();
//        assertThat(client.getAppointment().size()).isEqualTo(1);
//        assertThat(client.getAppointment().getFirst().getStatus()).isEqualTo(CANCELLED);
//    }
//    @Test
//    public void testThatClientCanDeleteAppointment(){
//        BookAppointmentRequest request = new BookAppointmentRequest();
//        request.setClientId(2L);
//        request.setCategory(Category.ELECTRICAL);
//        request.setStatus(AppointmentStatus.SCHEDULED);
//        request.setScheduleTime(java.time.LocalDateTime.now().plusDays(6));
//        request.setSkilledWorkerId(202L);
//        clientService.bookAppointment(request);
//        assertThat(clientRepository.findById(2L).get()
//                .getAppointment().size()).isEqualTo(1);
//
//    }
}
