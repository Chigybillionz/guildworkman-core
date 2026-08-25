# Notification Service: Persisted In-App Notifications & Email Fan-Out

Implements the Notification Service issue: appointment lifecycle changes
(booked, accepted, declined, updated, cancelled, deleted) now produce a
persisted, readable in-app notification for each affected party, plus a
best-effort transactional email — instead of silently changing state that
nobody is told about.

## What was there before

`NotificationServiceImpl` was an empty `@Service` implementing an empty
`NotificationService` interface. `Notification` (the JPA entity) and
`NotificationRepository` existed but were never referenced from anywhere else
in the codebase. `MailServiceImp` could already send transactional email
through Brevo, but nothing called it from the appointment flow. None of the
three pieces were connected, and no appointment lifecycle method — booking,
accepting, updating, cancelling, or deleting — told either party anything had
changed.

## New dependencies

None. Everything here — Spring's `@Async`/`@EnableAsync`, `TransactionSynchronizationManager`,
Spring Data JPA paging, springdoc — was already on the classpath.

## Architectural decisions

### 1. Email fan-out runs off the request thread, after the transaction commits

The issue requires that a slow or down mail provider never fail or block the
appointment operation, and that a failed send never leave a booking
half-committed. Two mechanisms together guarantee this:

- **A dedicated executor** (`AsyncConfig` / `NotificationEmailDispatcher`).
  `NotificationEmailDispatcher.dispatch` is `@Async` on its own bean —
  `@Async` only intercepts calls that arrive from outside the declaring bean,
  so it has to live on a class of its own rather than as a method on
  `NotificationServiceImpl` that method would call on itself.
- **Deferred until commit.** `NotificationServiceImpl#scheduleEmail` checks
  `TransactionSynchronizationManager.isSynchronizationActive()`. If the
  appointment write that produced this notification is still inside a
  transaction, the email is scheduled via `registerSynchronization(...).afterCommit()`
  rather than sent immediately — so a notification is never emailed for a
  change that ends up rolled back, and the email call is never made on the
  thread holding the database connection.

A failed or thrown-exception send is caught inside `dispatch` and recorded on
the notification's `emailStatus` (`PENDING`/`SENT`/`FAILED`); it is never
rethrown; there is nothing left to observe it by the time it runs, and
resurfacing it would have nowhere useful to go.

### 2. The in-app notification is not optional or best-effort

Unlike the email, persisting the `Notification` row happens synchronously, in
the same transaction as the appointment write. If the appointment save rolls
back, the notification never existed either; if it commits, the notification
is guaranteed to exist before the (still-pending) email is even scheduled.

### 3. Ownership is scoped by email, not by a linked user id

`Client`, `SkilledWorker`, and `UserAccount` (the JWT login identity) are
deliberately separate tables in this codebase with no foreign key between
them (see `UserAccount`'s class Javadoc) — email is the only identifier they
share, and it is what `JwtAuthenticationFilter` puts in the security
principal. `Notification.recipientEmail` is scoped the same way:
`NotificationController` reads the caller's email via
`@AuthenticationPrincipal String`, and every repository query is filtered by
it — `GET /api/v1/notifications`, `GET /api/v1/notifications/unread-count`,
`PUT /api/v1/notifications/{id}/read`, and `PUT /api/v1/notifications/read-all`
all only ever see or touch the caller's own rows.  A notification id that
exists but belongs to someone else 404s exactly like one that doesn't exist,
so a caller can't distinguish "not mine" from "never existed".

### 4. A pre-existing gap this depended on: booking never resolved a `Client`

`AppointmentServiceImpl#bookAppointment` mapped `BookAppointmentRequest` to
`Appointment` via `ModelMapper` and explicitly resolved the `SkilledWorker` by
id (with a comment explaining why: ModelMapper can silently build a transient
entity from just an id, which either fails to persist or corrupts the row) —
but it never did the same for the `Client`. Booking notifications need a real
`client.getEmail()`/`getFullName()`, so `bookAppointment` now resolves the
`Client` from `clientId` the same way it already did for the worker.
`ClientServiceImpl#bookAppointment` still re-sets and re-saves the client
afterwards; that's now redundant but harmless, and left alone to minimize the
blast radius of this change.

### 5. Two appointment-creation paths, one notification call

Appointments are created in two places: the legacy
`AppointmentServiceImpl#bookAppointment` (`POST /api/v1/client/bookAppointment`)
and the concurrency-safe two-step `SlotReservationService#confirm`
(`POST /api/v1/booking/reservations/{id}/confirm`, see `APPOINTMENT_BOOKING.md`).
Both now call `NotificationService#notifyAppointmentEvent(appointment,
APPOINTMENT_BOOKED)` at their respective points of success, so a booking
produces a notification regardless of which flow made it.

## Schema / migrations

No migration file, and none is needed: this codebase has no Flyway/Liquibase
and manages schema via `spring.jpa.hibernate.ddl-auto=update`
(`application.properties`). `Notification` already existed as an unused
table, so this PR is additive on top of it rather than a fresh table:

- Every new column (`recipient_email`, `recipient_role`, `type`, `title`,
  `message`, `appointment_id`, `is_read`, `read_at`, `email_status`,
  `created_at`) is new — `update` mode creates them on first deploy.
- The identity column deliberately **keeps its original name**
  (`notification_id`, from the pre-existing `notificationId` field) instead
  of being renamed to `id` to match newer entities in this codebase.
  `ddl-auto=update` only ever adds missing columns; it does not rename or
  drop them, so renaming the primary key column could leave two identity
  columns behind on an already-deployed database. The old, now-unused
  `user_id`/`status` columns are left in place for the same reason — inert,
  but not worth an ALTER TABLE this schema-management strategy can't express
  safely. See `docs/ESCROW_ORCHESTRATION.md`/`docs/APPOINTMENT_BOOKING.md` for
  the same tradeoff made elsewhere in this codebase.

## Endpoints

All under `/api/v1/notifications`, authenticated (JWT bearer), scoped to the
caller:

| Method & path                          | Purpose                                    |
|-----------------------------------------|---------------------------------------------|
| `GET /`                                 | Paginated list, newest first                |
| `GET /unread-count`                     | Count of unread notifications                |
| `PUT /{id}/read`                        | Mark one notification read (404 if not the caller's) |
| `PUT /read-all`                         | Mark all of the caller's unread notifications read |

Documented via springdoc (`@Operation`); errors follow the same RFC 7807
(`application/problem+json`) contract as the rest of the API — a new
`NotificationNotFoundException` → 404 entry was added to
`GlobalExceptionHandler`.

## Pagination

`GET /api/v1/notifications` defaults to a page size of 20
(`@PageableDefault` on `NotificationController#list`) and is now hard-capped
at 100 regardless of what a caller passes as `?size=`
(`spring.data.web.pageable.max-page-size`, `application.properties`) — Spring
Data's own default cap (2000) was too high to call safe for a public list
endpoint. Sort defaults to `createdAt DESC`, matching the
`idx_notifications_recipient_created` index below, so the common case never
falls back to a filesort.

## Indexing & retention

`Notification` carries two indexes matching its two access patterns:
`idx_notifications_recipient_created` (`recipient_email, created_at`) for the
paginated list, and `idx_notifications_recipient_unread`
(`recipient_email, is_read`) for the unread count and `markAllAsRead`'s
lookup.

There is no retention/archival job. This table now gets a row per recipient
per lifecycle event, so it grows unboundedly with appointment volume — that's
an acceptable starting point for this issue's scope, but worth flagging as
follow-up work before volume makes it a problem: either a scheduled sweep
that deletes/archives read notifications past some age (mirroring
`SlotReservationService#expireLapsedHolds`'s `@Scheduled` pattern), or a
TTL-based partitioning strategy if this grows enough to matter operationally.

## Observability

A failed email send is recorded two ways: the notification's own
`emailStatus=FAILED` (queryable directly), and an `ERROR`-level log line from
`NotificationEmailDispatcher` with the notification id, recipient, and
exception. There is no emitted metric/counter — this codebase has no
Micrometer/Actuator dependency today, and adding one is a cross-cutting,
new-dependency decision that belongs in its own PR rather than folding into
this feature. Log-based alerting on `NotificationEmailDispatcher` at `ERROR`
is the interim path to ops visibility.

## Concurrency

`Notification` intentionally has no `@Version`/optimistic-locking column. The
only post-insert mutations are `read` flipping `false → true` and
`emailStatus` being set exactly once by the dispatcher; both are idempotent
and monotonic, and every read/write is already scoped by `recipientEmail`
first. Two concurrent mark-read calls on the same row converge on the same
end state, so optimistic locking would only risk throwing on a harmless race,
not prevent a lost update.

## Testing

- `NotificationServiceImplTest` — recipient selection per lifecycle event
  (e.g. accepted/declined only notify the client), content, pagination
  delegation, and that mark-read/mark-all-read are scoped to the caller's
  email.
- `NotificationEmailDispatcherTest` — the mail-provider-down case: a thrown
  exception from `MailService` is caught, marks the notification `FAILED`,
  and never propagates.
- `AppointmentServiceImplTest` — each lifecycle method
  (book/cancel/update/accept/delete) calls `notifyAppointmentEvent` with the
  correct `NotificationType`.
- `NotificationControllerTest` — HTTP-level ownership scoping (list,
  unread-count, mark-read, mark-all-read) against a real JWT and a real
  database. Cross-user access returns `404`, not `403`: recipient scoping is
  by `recipientEmail`, not by an id the caller passes, so there's no
  "wrong owner, right id" case to distinguish from "no such notification" —
  see architectural decision #3.
- `NotificationEmailFanOutIntegrationTest` — end-to-end proof (real
  `AppointmentService`, real transaction, real `@Async` executor, mocked
  `MailService` throwing) that a down mail provider still lets
  `bookAppointment` commit, and that the resulting notifications land as
  `emailStatus=FAILED` rather than blocking or rolling back the booking.
- Existing booking tests (`SlotReservationIntegrationTest`,
  `BookingControllerTest`, `ClientServiceTest`) now mock `MailService` so
  they don't make real calls to the mail provider now that booking triggers
  a notification fan-out.

## CI

`test.yml` already caches Maven dependencies (`cache: maven` on
`actions/setup-java@v4`) — no change needed there.
