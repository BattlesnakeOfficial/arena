ALTER TABLE notifications ADD COLUMN read_at TIMESTAMPTZ;

CREATE INDEX notifications_unread_badge_idx
    ON notifications (user_id, created_at DESC, notification_id DESC)
    WHERE in_app_visible AND read_at IS NULL;
