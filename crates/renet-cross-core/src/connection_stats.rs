use std::time::Duration;

const RESOLUTION: Duration = Duration::from_millis(300);
const WINDOW: Duration = Duration::from_millis(6000);
const SIZE: usize = (WINDOW.as_millis() / RESOLUTION.as_millis()) as usize;

#[derive(Debug, Default)]
pub struct ConnectionStats {
    packets_sent: [u64; SIZE],
    packets_acked: [u64; SIZE],
    bytes_sent: [u64; SIZE],
    bytes_received: [u64; SIZE],
    current_index: usize,
    current_bucket: u128,
}

impl ConnectionStats {
    pub fn new() -> Self {
        Self {
            packets_sent: [0; SIZE],
            packets_acked: [0; SIZE],
            bytes_sent: [0; SIZE],
            bytes_received: [0; SIZE],
            current_index: 0,
            current_bucket: 0,
        }
    }

    fn index(time: Duration) -> usize {
        (time.as_millis() / RESOLUTION.as_millis()) as usize % SIZE
    }

    pub fn update(&mut self, current_time: Duration) {
        let bucket = current_time.as_millis() / RESOLUTION.as_millis();
        // Clear every crossed bucket, including a full wrap back to the same
        // index. Updates can skip resolutions when a client is suspended.
        for offset in 1..=(bucket - self.current_bucket).min(SIZE as u128) {
            let i = (self.current_index + offset as usize) % SIZE;
            self.packets_sent[i] = 0;
            self.bytes_sent[i] = 0;
            self.bytes_received[i] = 0;
            self.packets_acked[i] = 0;
        }
        self.current_index = Self::index(current_time);
        self.current_bucket = bucket;
    }

    pub fn sent_packets(&mut self, num_packets: u64, bytes: u64) {
        self.packets_sent[self.current_index] += num_packets;
        self.bytes_sent[self.current_index] += bytes;
    }

    pub fn received_packet(&mut self, bytes: u64) {
        self.bytes_received[self.current_index] += bytes;
    }

    pub fn acked_packet(&mut self, sent_at: Duration, current_time: Duration) {
        let sent_bucket = sent_at.as_millis() / RESOLUTION.as_millis();
        if sent_at > current_time || sent_bucket > self.current_bucket || self.current_bucket - sent_bucket >= SIZE as u128 {
            // Bucket lifetime is measured from its start, not the packet's
            // send time. Never credit an ACK to a recycled bucket.
            return;
        }

        self.packets_acked[Self::index(sent_at)] += 1;
    }

    pub fn bytes_sent_per_second(&self, current_time: Duration) -> f64 {
        let mut total_bytes: u64 = self.bytes_sent.iter().sum();

        if current_time < WINDOW {
            return total_bytes as f64 / current_time.as_secs_f64();
        }

        // Ignore the current incomplete resolution
        total_bytes -= self.bytes_sent[self.current_index];

        total_bytes as f64 / (WINDOW - RESOLUTION).as_secs_f64()
    }

    pub fn bytes_received_per_second(&self, current_time: Duration) -> f64 {
        let mut total_bytes: u64 = self.bytes_received.iter().sum();

        if current_time < WINDOW {
            return total_bytes as f64 / current_time.as_secs_f64();
        }

        // Ignore the current incomplete resolution
        total_bytes -= self.bytes_received[self.current_index];
        total_bytes as f64 / (WINDOW - RESOLUTION).as_secs_f64()
    }

    pub fn packet_loss(&self) -> f64 {
        let total_packets_sent = {
            let mut sum: u64 = self.packets_sent.iter().sum();

            // Ignore the current and last 2 resolutions,
            // because the message or its ack could be in flight
            sum -= self.packets_sent[self.current_index];
            sum -= self.packets_sent[(self.current_index + SIZE - 1) % SIZE];
            sum -= self.packets_sent[(self.current_index + SIZE - 2) % SIZE];
            sum as f64
        };

        let total_packets_acked = {
            let mut sum: u64 = self.packets_acked.iter().sum();
            sum -= self.packets_acked[self.current_index];
            sum -= self.packets_acked[(self.current_index + SIZE - 1) % SIZE];
            sum -= self.packets_acked[(self.current_index + SIZE - 2) % SIZE];
            sum as f64
        };

        if total_packets_sent == 0.0 {
            return 0.0;
        }

        (total_packets_sent - total_packets_acked) / total_packets_sent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skipped_resolutions_expire_all_old_samples() {
        let mut stats = ConnectionStats::new();
        stats.sent_packets(1, 10);
        stats.received_packet(20);
        stats.update(Duration::from_millis(300));
        stats.sent_packets(1, 30);
        stats.received_packet(40);
        stats.update(Duration::from_millis(6300));
        assert_eq!(stats.packet_loss(), 0.0);
        assert_eq!(stats.bytes_sent_per_second(Duration::from_millis(6300)), 0.0);
        assert_eq!(stats.bytes_received_per_second(Duration::from_millis(6300)), 0.0);
    }

    #[test]
    fn skipped_partial_wrap_expires_crossed_buckets() {
        let mut stats = ConnectionStats::new();
        stats.sent_packets(1, 10);
        stats.update(Duration::from_millis(300));
        stats.sent_packets(1, 20);
        stats.update(Duration::from_millis(5700));
        stats.sent_packets(1, 30);
        stats.update(Duration::from_millis(6300));
        assert_eq!(stats.packets_sent.iter().sum::<u64>(), 1);
        assert_eq!(stats.bytes_sent.iter().sum::<u64>(), 30);
    }

    #[test]
    fn late_ack_cannot_credit_a_recycled_bucket() {
        let mut stats = ConnectionStats::new();
        let sent_at = Duration::from_millis(299);
        stats.update(sent_at);
        stats.sent_packets(1, 10);
        let now = Duration::from_millis(6000);
        stats.update(now);
        stats.sent_packets(1, 20);
        // Less than WINDOW old, but its original bucket has already expired.
        stats.acked_packet(sent_at, now);
        stats.acked_packet(Duration::ZERO, now);
        stats.update(Duration::from_millis(6900));
        assert_eq!(stats.packet_loss(), 1.0);
        stats.acked_packet(now, Duration::from_millis(6900));
        assert_eq!(stats.packet_loss(), 0.0);
    }

    #[test]
    fn bytes_per_sec() {
        let mut current_time = Duration::ZERO;
        let mut window = ConnectionStats::default();

        for _ in 0..10 {
            window.update(current_time);
            window.sent_packets(10, 100);
            current_time += Duration::from_millis(100);
        }

        // Check at 1 second
        assert_eq!(window.bytes_sent_per_second(current_time), 1000.);

        for _ in 0..50 {
            window.update(current_time);
            window.sent_packets(10, 100);
            current_time += Duration::from_millis(100);
        }

        // Check after 6 seconds
        assert_eq!(window.packets_sent, [30; SIZE]);
        assert_eq!(window.bytes_sent, [300; SIZE]);
        assert_eq!(window.bytes_sent_per_second(current_time), 1000.);
    }

    #[test]
    fn packet_loss() {
        let mut current_time = Duration::ZERO;
        let mut window = ConnectionStats::default();

        for _ in 0..20 {
            window.update(current_time);
            // Send 2, ack only 1
            window.sent_packets(2, 100);
            window.acked_packet(current_time, current_time);
            current_time += Duration::from_millis(100);
        }

        // Check at 2 second
        assert_eq!(window.packet_loss(), 0.5);

        for _ in 0..40 {
            window.update(current_time);
            window.sent_packets(2, 100);
            window.acked_packet(current_time, current_time);
            current_time += Duration::from_millis(100);
        }

        // Check after 6 seconds
        assert_eq!(window.packets_sent, [6; SIZE]);
        assert_eq!(window.packets_acked, [3; SIZE]);
        assert_eq!(window.packet_loss(), 0.5);
    }
}
