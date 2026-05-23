use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use zbus::{interface, connection};
use zbus::zvariant::Value;

struct Notifications {
    counter: AtomicU32,
}

#[interface(name = "org.freedesktop.Notifications")]
impl Notifications {
    async fn get_capabilities(&self) -> Vec<String> {
        vec![
            "body".to_string(),
            "actions".to_string(),
            "icon-static".to_string(),
        ]
    }

    async fn notify(
        &self,
        app_name: String,
        replaces_id: u32,
        app_icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        hints: HashMap<String, Value<'_>>,
        expire_timeout: i32,
    ) -> u32 {
        let id = if replaces_id == 0 {
            self.counter.fetch_add(1, Ordering::SeqCst)
        } else {
            replaces_id
        };

        println!(
            "[clear-notifier] Notification #{} from {}:\n  Summary: {}\n  Body: {}\n  Icon: {}\n  Actions: {:?}\n  Hints: {:?}\n  Timeout: {}ms",
            id, app_name, summary, body, app_icon, actions, hints, expire_timeout
        );

        id
    }

    async fn close_notification(&self, id: u32) {
        println!("[clear-notifier] Closing notification #{}", id);
    }

    async fn get_server_information(&self) -> (String, String, String, String) {
        (
            "clear-notifier".to_string(),
            "ClearWM Project".to_string(),
            "0.1.0".to_string(),
            "1.2".to_string(),
        )
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let notifications = Notifications {
        counter: AtomicU32::new(1),
    };

    let _connection = connection::Builder::session()?
        .name("org.freedesktop.Notifications")?
        .serve_at("/org/freedesktop/Notifications", notifications)?
        .build()
        .await?;

    println!("[clear-notifier] Service registered. Listening for notifications...");

    // Keep the process running
    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(3600)).await;
    }
}
