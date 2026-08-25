package com.guildworkman.api.booking.service;

import com.guildworkman.api.booking.api.ReserveSlotRequest;
import com.guildworkman.api.booking.model.SlotReservation;
import com.guildworkman.api.booking.model.SlotReservationStatus;
import com.guildworkman.api.booking.repository.SlotReservationRepository;
import com.guildworkman.api.data.repository.AppointmentRepository;
import com.guildworkman.api.data.repository.ClientRepository;
import com.guildworkman.api.data.repository.SkilledWorkerRepository;
import com.guildworkman.api.services.ServiceUtils.NotificationService;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.extension.ExtendWith;
import org.mockito.InjectMocks;
import org.mockito.Mock;
import org.mockito.junit.jupiter.MockitoExtension;
import org.springframework.dao.DataIntegrityViolationException;

import java.time.Duration;
import java.time.Instant;
import java.time.LocalDateTime;
import java.util.List;
import java.util.Optional;

import static org.assertj.core.api.Assertions.assertThat;
import static org.assertj.core.api.Assertions.assertThatThrownBy;
import static org.mockito.ArgumentMatchers.any;
import static org.mockito.Mockito.never;
import static org.mockito.Mockito.verify;
import static org.mockito.Mockito.when;

/**
 * Unit coverage for the decisions {@link SlotReservationService} makes on its
 * own — idempotent replay, recovering from a lost unique-index race, and the
 * state checks around confirm/release. The guarantee those sit on top of is
 * database behaviour and is covered in
 * {@code com.guildworkman.api.booking.SlotReservationIntegrationTest}.
 */
@ExtendWith(MockitoExtension.class)
class SlotReservationServiceTest {

    @Mock
    private SlotReservationBooker booker;
    @Mock
    private SlotReservationRepository reservations;
    @Mock
    private AppointmentRepository appointments;
    @Mock
    private ClientRepository clients;
    @Mock
    private SkilledWorkerRepository skilledWorkers;
    @Mock
    private SlotReservationProperties properties;
    @Mock
    private NotificationService notificationService;

    @InjectMocks
    private SlotReservationService service;

    private ReserveSlotRequest request;

    @BeforeEach
    void setUp() {
        request = new ReserveSlotRequest("idem-1", 7L, 3L, LocalDateTime.now().plusDays(1), 60);
    }

    private static SlotReservation reservation(Long id, SlotReservationStatus status) {
        SlotReservation reservation = new SlotReservation();
        reservation.setId(id);
        reservation.setIdempotencyKey("idem-1");
        reservation.setSkilledWorkerId(7L);
        reservation.setClientId(3L);
        reservation.setSlotStart(LocalDateTime.now().plusDays(1));
        reservation.setSlotEnd(LocalDateTime.now().plusDays(1).plusHours(1));
        reservation.setStatus(status);
        if (status == SlotReservationStatus.HELD) {
            reservation.setExpiresAt(Instant.now().plus(Duration.ofMinutes(5)));
        }
        return reservation;
    }

    // --- reserve -------------------------------------------------------------

    @Test
    void aKnownIdempotencyKeyReplaysWithoutTakingAnotherHold() {
        SlotReservation existing = reservation(1L, SlotReservationStatus.HELD);
        when(reservations.findByIdempotencyKey("idem-1")).thenReturn(Optional.of(existing));

        var outcome = service.reserve(request);

        assertThat(outcome.replayed()).isTrue();
        assertThat(outcome.reservation()).isSameAs(existing);
        verify(booker, never()).hold(any());
    }

    @Test
    void losingTheIdempotencyKeyRaceReplaysTheWinner() {
        SlotReservation winner = reservation(2L, SlotReservationStatus.HELD);
        when(reservations.findByIdempotencyKey("idem-1"))
                .thenReturn(Optional.empty())
                .thenReturn(Optional.of(winner));
        when(booker.hold(request)).thenThrow(new DataIntegrityViolationException("duplicate key"));

        var outcome = service.reserve(request);

        // A concurrent retry of the caller's own request won: that row is the
        // answer, not a conflict.
        assertThat(outcome.replayed()).isTrue();
        assertThat(outcome.reservation()).isSameAs(winner);
    }

    @Test
    void anIntegrityViolationWithNoMatchingKeyMeansTheSlotWentToSomeoneElse() {
        when(reservations.findByIdempotencyKey("idem-1")).thenReturn(Optional.empty());
        when(booker.hold(request)).thenThrow(new DataIntegrityViolationException("duplicate active_slot_key"));

        // Nothing exists for this idempotency key, so the constraint that fired
        // was the active-slot backstop — somebody claimed the slot.
        assertThatThrownBy(() -> service.reserve(request)).isInstanceOf(SlotUnavailableException.class);
    }

    @Test
    void aSuccessfulHoldIsNotFlaggedAsAReplay() {
        SlotReservation held = reservation(3L, SlotReservationStatus.HELD);
        when(reservations.findByIdempotencyKey("idem-1")).thenReturn(Optional.empty());
        when(booker.hold(request)).thenReturn(held);

        var outcome = service.reserve(request);

        assertThat(outcome.replayed()).isFalse();
        assertThat(outcome.reservation()).isSameAs(held);
    }

    // --- confirm --------------------------------------------------------------

    @Test
    void confirmingAnUnknownReservationIsNotFound() {
        when(reservations.findByIdForUpdate(42L)).thenReturn(Optional.empty());

        assertThatThrownBy(() -> service.confirm(42L, null))
                .isInstanceOf(ReservationNotFoundException.class);
    }

    @Test
    void confirmingAnAlreadyConfirmedReservationReturnsItUnchanged() {
        SlotReservation confirmed = reservation(4L, SlotReservationStatus.CONFIRMED);
        confirmed.setAppointmentId(99L);
        when(reservations.findByIdForUpdate(4L)).thenReturn(Optional.of(confirmed));

        SlotReservation result = service.confirm(4L, null);

        assertThat(result.getAppointmentId()).isEqualTo(99L);
        verify(appointments, never()).saveAndFlush(any());
    }

    @Test
    void confirmingAReleasedReservationIsRejected() {
        when(reservations.findByIdForUpdate(5L))
                .thenReturn(Optional.of(reservation(5L, SlotReservationStatus.RELEASED)));

        assertThatThrownBy(() -> service.confirm(5L, null))
                .isInstanceOf(ReservationNotHeldException.class);
        verify(appointments, never()).saveAndFlush(any());
    }

    @Test
    void confirmingALapsedHoldIsRejectedWithoutBookingAnything() {
        SlotReservation lapsed = reservation(6L, SlotReservationStatus.HELD);
        lapsed.setExpiresAt(Instant.now().minusSeconds(1));
        when(reservations.findByIdForUpdate(6L)).thenReturn(Optional.of(lapsed));

        assertThatThrownBy(() -> service.confirm(6L, null))
                .isInstanceOf(ReservationNotHeldException.class);

        // Nothing is written: the rejection rolls the transaction back, so the
        // relabelling to EXPIRED is left to the reserve path or the sweep.
        verify(appointments, never()).saveAndFlush(any());
        verify(reservations, never()).save(any());
    }

    // --- release ----------------------------------------------------------------

    @Test
    void releasingAHoldClearsItsActiveSlotKey() {
        SlotReservation held = reservation(7L, SlotReservationStatus.HELD);
        held.setActiveSlotKey("7@slot");
        when(reservations.findByIdForUpdate(7L)).thenReturn(Optional.of(held));

        service.release(7L);

        assertThat(held.getStatus()).isEqualTo(SlotReservationStatus.RELEASED);
        assertThat(held.getActiveSlotKey()).isNull();
        assertThat(held.getReleasedAt()).isNotNull();
    }

    @Test
    void releasingAnAlreadyReleasedReservationIsANoOp() {
        SlotReservation released = reservation(8L, SlotReservationStatus.RELEASED);
        when(reservations.findByIdForUpdate(8L)).thenReturn(Optional.of(released));

        SlotReservation result = service.release(8L);

        assertThat(result.getStatus()).isEqualTo(SlotReservationStatus.RELEASED);
        verify(reservations, never()).save(any());
    }

    @Test
    void releasingAConfirmedReservationIsRejected() {
        when(reservations.findByIdForUpdate(9L))
                .thenReturn(Optional.of(reservation(9L, SlotReservationStatus.CONFIRMED)));

        // Freeing a booked slot means cancelling the appointment, which is what
        // releaseForAppointment is for.
        assertThatThrownBy(() -> service.release(9L)).isInstanceOf(ReservationNotHeldException.class);
    }

    @Test
    void cancellingAnAppointmentReleasesTheSlotItOccupied() {
        SlotReservation confirmed = reservation(10L, SlotReservationStatus.CONFIRMED);
        confirmed.setActiveSlotKey("7@slot");
        confirmed.setAppointmentId(55L);
        when(reservations.findByAppointmentIdAndStatus(55L, SlotReservationStatus.CONFIRMED))
                .thenReturn(List.of(confirmed));

        service.releaseForAppointment(55L);

        assertThat(confirmed.getStatus()).isEqualTo(SlotReservationStatus.RELEASED);
        assertThat(confirmed.getActiveSlotKey()).isNull();
        verify(reservations).save(confirmed);
    }
}
