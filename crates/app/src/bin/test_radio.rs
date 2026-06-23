use std::sync::Arc;
use clawdpanel_media::{RadioEngine, Event};

fn main() {
    std::env::set_var("GST_DEBUG", "souphttpsrc:5,2");

    let cfg = clawdpanel_claude_core::config::load();
    println!("Loaded config: active_station={}, radio_volume={}, stations={}", 
        cfg.active_station, cfg.radio_volume, cfg.stations.len());
    
    let emit = Arc::new(|ev: Event| {
        println!("UI Event: state={:?}, progress={}, position={}, duration={}, err={}", 
            ev.state, ev.progress, ev.position, ev.duration, ev.err);
    });
    
    println!("Initializing RadioEngine...");
    let engine = match RadioEngine::new(cfg.stations.clone(), cfg.radio_volume, emit) {
        Ok(e) => e,
        Err(e) => {
            println!("Failed to create RadioEngine: {}", e);
            return;
        }
    };
    
    println!("Playing station 1 (LOFI GIRL)...");
    if let Err(e) = engine.play_station(1) {
        println!("Failed to play station: {}", e);
        return;
    }
    
    println!("Waiting for 90 seconds to stream...");
    std::thread::sleep(std::time::Duration::from_secs(90));
    println!("Finished test_radio!");
}
