package com.guildworkman.api.data.repository;

import com.guildworkman.api.data.models.Notification;
import org.springframework.data.domain.Page;
import org.springframework.data.domain.Pageable;
import org.springframework.data.jpa.repository.JpaRepository;

import java.util.List;
import java.util.Optional;

public interface NotificationRepository extends JpaRepository<Notification, Long> {

    Page<Notification> findByRecipientEmail(String recipientEmail, Pageable pageable);

    long countByRecipientEmailAndReadFalse(String recipientEmail);

    Optional<Notification> findByNotificationIdAndRecipientEmail(Long notificationId, String recipientEmail);

    List<Notification> findByRecipientEmailAndReadFalse(String recipientEmail);
}
