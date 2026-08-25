package com.guildworkman.api.config;

import org.springframework.context.annotation.Bean;
import org.springframework.context.annotation.Configuration;
import org.springframework.scheduling.annotation.EnableAsync;
import org.springframework.scheduling.concurrent.ThreadPoolTaskExecutor;

import java.util.concurrent.Executor;

/**
 * Backing executor for the notification fan-out email. Dedicated and small on
 * purpose: mail-provider calls are the one thing in this app that must never
 * share a thread pool with request handling, since a slow or down provider
 * (see {@code NotificationEmailDispatcher}) must not be able to starve
 * anything else.
 */
@Configuration
@EnableAsync
public class AsyncConfig {

    public static final String NOTIFICATION_MAIL_EXECUTOR = "notificationMailExecutor";

    @Bean(NOTIFICATION_MAIL_EXECUTOR)
    public Executor notificationMailExecutor() {
        ThreadPoolTaskExecutor executor = new ThreadPoolTaskExecutor();
        executor.setCorePoolSize(2);
        executor.setMaxPoolSize(8);
        executor.setQueueCapacity(500);
        executor.setThreadNamePrefix("notif-mail-");
        executor.initialize();
        return executor;
    }
}
