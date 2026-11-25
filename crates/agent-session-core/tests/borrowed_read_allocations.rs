//! Borrowed reads copy no content (spec: Native Rust library / Reading a turn
//! copies no content). This binary installs a counting global allocator, so
//! it holds only this test.

use querymt_agent_session_core::SessionCore;
use querymt_agent_session_core::entry::EntryKind;
use querymt_agent_session_core::testing::{live, text_chunk};
use serde_json::json;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    static ALLOCATED: Cell<u64> = const { Cell::new(0) };
}

struct Counting;

// SAFETY: every call is forwarded unchanged to the system allocator; the
// counter is a const-initialised thread-local `Cell`, which never allocates.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATED.try_with(|total| total.set(total.get() + layout.size() as u64));
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _ = ALLOCATED.try_with(|total| total.set(total.get() + new_size as u64));
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Bytes allocated on this thread while `work` runs.
fn allocated_by<T>(work: impl FnOnce() -> T) -> (T, u64) {
    let before = ALLOCATED.with(Cell::get);
    let value = work();
    (value, ALLOCATED.with(Cell::get) - before)
}

const RESULT_BYTES: usize = 400 * 1024;

#[test]
fn reading_a_turn_copies_no_content() {
    let mut core = SessionCore::new("s1");
    core.apply(&[live(json!({
        "sessionUpdate": "tool_call",
        "toolCallId": "t1",
        "title": "Run shell",
        "status": "completed",
        "rawOutput": "x".repeat(RESULT_BYTES)
    }))]);

    let mut read_bytes = 0u64;
    let mut seen_text = 0usize;
    for _ in 0..100 {
        core.apply(&[text_chunk("m1", "lorem ")]);
        let ((entries, text, tool_output), bytes) = allocated_by(|| {
            let last = core.turn_order().last().expect("a turn");
            let turn = core.turn_ref(last).expect("turn exists");
            let (mut entries, mut text, mut tool_output) = (0usize, 0usize, 0usize);
            for entry in turn.entries() {
                entries += 1;
                match &entry.kind {
                    EntryKind::AssistantMessage(message) => text = message.text.len(),
                    EntryKind::ToolCall(call) => {
                        tool_output = call
                            .raw_output
                            .as_ref()
                            .and_then(|value| value.as_str())
                            .map_or(0, str::len);
                    }
                    _ => {}
                }
            }
            // An entry is also reachable by id, without scanning the turns.
            assert!(core.entry("tool-t1").is_some());
            (entries, text, tool_output)
        });
        read_bytes += bytes;
        seen_text = text;
        assert_eq!(entries, 2);
        assert_eq!(tool_output, RESULT_BYTES);
        assert!(
            bytes < 1024,
            "a borrowed read allocated {bytes} bytes; it must not depend on the content size"
        );
    }
    assert_eq!(seen_text, "lorem ".len() * 100);
    assert!(
        read_bytes < 100 * 1024,
        "100 reads allocated {read_bytes} bytes"
    );

    // Control: the owned read does copy the result, so the bound above is
    // measuring something.
    let (_, owned_bytes) = allocated_by(|| {
        let last = core.turn_order().last().expect("a turn").to_owned();
        core.turn(&last).expect("turn exists")
    });
    assert!(
        owned_bytes >= RESULT_BYTES as u64,
        "the owned read allocated only {owned_bytes} bytes"
    );
}
