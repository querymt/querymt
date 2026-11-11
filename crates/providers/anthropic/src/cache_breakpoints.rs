//! Enforces Anthropic's prompt-cache breakpoint rules on a built request.
//!
//! Anthropic rejects a request with more than [`MAX_CACHE_BREAKPOINTS`]
//! `cache_control` markers, and rejects a marker whose lifetime is longer
//! than that of a marker after it. Callers can request any number of hints,
//! so the rules are applied once, on the final wire blocks.

use crate::wire::request::AnthropicMessage;
use crate::wire::system::{AnthropicSystemPrompt, CacheControlEphemeral, CacheTTL};

/// Maximum number of `cache_control` markers Anthropic accepts per request.
pub(crate) const MAX_CACHE_BREAKPOINTS: usize = 4;

/// Order in which marker positions are kept, most valuable first.
///
/// Positions index the markers in request order: `0..system_count` are system
/// markers, `system_count..total` are message markers.
///
/// 1. last system marker (covers tools and the whole system prompt);
/// 2. last two message markers (per-step reuse; the second-to-last also
///    covers turns longer than Anthropic's ~20 block lookback);
/// 3. first system marker;
/// 4. remaining message markers, newest first;
/// 5. remaining system markers, last first.
///
/// Positions can repeat; callers keep the first occurrence of each.
fn keep_priority(system_count: usize, total: usize) -> impl Iterator<Item = usize> {
    let system = 0..system_count;
    let messages = system_count..total;
    system
        .clone()
        .next_back()
        .into_iter()
        .chain(messages.clone().rev().take(2))
        .chain(system.clone().next())
        .chain(messages.rev().skip(2))
        .chain(system.rev())
}

/// Limits `system` and `messages` to [`MAX_CACHE_BREAKPOINTS`] markers and
/// makes marker lifetimes non-increasing in request order.
///
/// Only markers that are actually on a block count, so hints that produced no
/// marker never use up the budget. Excess markers are cleared in the priority
/// given by [`keep_priority`]; afterwards every surviving marker is raised to
/// the longest lifetime that follows it. Raising happens after dropping so a
/// removed marker never influences a lifetime.
///
/// Returns the number of markers dropped.
pub(crate) fn normalize_cache_breakpoints(
    system: Option<&mut AnthropicSystemPrompt>,
    messages: &mut [AnthropicMessage],
) -> usize {
    let mut slots: Vec<&mut Option<CacheControlEphemeral>> = Vec::new();
    if let Some(AnthropicSystemPrompt::Blocks(blocks)) = system {
        slots.extend(
            blocks
                .iter_mut()
                .map(|block| &mut block.cache_control)
                .filter(|slot| slot.is_some()),
        );
    }
    let system_count = slots.len();
    slots.extend(
        messages
            .iter_mut()
            .flat_map(|message| message.content.iter_mut())
            .filter_map(|block| block.cache_control_mut())
            .filter(|slot| slot.is_some()),
    );

    let total = slots.len();
    let mut dropped = 0;
    if total > MAX_CACHE_BREAKPOINTS {
        let mut keep = vec![false; total];
        let mut kept = 0;
        for position in keep_priority(system_count, total) {
            if !keep[position] {
                keep[position] = true;
                kept += 1;
                if kept == MAX_CACHE_BREAKPOINTS {
                    break;
                }
            }
        }
        for (slot, keep) in slots.iter_mut().zip(&keep) {
            if !keep {
                **slot = None;
            }
        }
        dropped = total - kept;
    }

    // Walk from the end: once a one-hour marker is seen, every earlier marker
    // must be at least as long.
    let mut one_hour_follows = false;
    for control in slots.iter_mut().rev().filter_map(|slot| slot.as_mut()) {
        if control.ttl == Some(CacheTTL::OneHour) {
            one_hour_follows = true;
        } else if one_hour_follows {
            control.ttl = Some(CacheTTL::OneHour);
        }
    }

    dropped
}
