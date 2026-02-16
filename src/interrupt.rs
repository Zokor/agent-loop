use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, Ordering},
};

static INTERRUPTED: OnceLock<Arc<AtomicBool>> = OnceLock::new();

fn flag() -> &'static Arc<AtomicBool> {
    INTERRUPTED.get_or_init(|| Arc::new(AtomicBool::new(false)))
}

pub fn register_signal_handlers() {
    let flag = Arc::clone(flag());

    #[cfg(unix)]
    {
        // SIGINT
        let _ = signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&flag));
        // SIGTERM
        let _ = signal_hook::flag::register(signal_hook::consts::SIGTERM, flag);
    }

    #[cfg(not(unix))]
    {
        // On non-unix platforms, signal-hook is not available.
        // Use ctrlc as a fallback if needed in the future.
        let _ = flag;
    }
}

pub fn is_interrupted() -> bool {
    flag().load(Ordering::Relaxed)
}

#[cfg(test)]
pub fn clear_interrupt_flag() {
    flag().store(false, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupt_flag_defaults_to_false() {
        // Ensure flag starts as false (or was cleared).
        clear_interrupt_flag();
        assert!(!is_interrupted());
    }

    #[test]
    fn interrupt_flag_can_be_set_and_cleared() {
        clear_interrupt_flag();
        assert!(!is_interrupted());

        flag().store(true, Ordering::Relaxed);
        assert!(is_interrupted());

        clear_interrupt_flag();
        assert!(!is_interrupted());
    }
}
