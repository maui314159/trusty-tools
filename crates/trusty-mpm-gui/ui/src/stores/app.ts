// Why: A single source of truth for fleet state shared by every component —
// the sidebar, detail panel, and event feed all read from these stores rather
// than fetching independently, so the UI stays consistent.
// What: Writable stores for sessions, the active selection, daemon health, and
// the recent-events buffer, plus a `refreshSessions()` poller and the shared
// `Session` / `HookEvent` types.
// Test: Call `refreshSessions()` with the daemon up and assert `sessions` is
// populated and `daemonHealth` becomes `'ok'`; with the daemon down assert it
// becomes `'error'`.

import { writable } from 'svelte/store';
import { invoke } from '../lib/transport';

/** A single managed session as reported by `GET /sessions`. */
export interface Session {
  id: string;
  workdir: string;
  status: 'running' | 'paused' | 'stopped' | 'awaiting_approval';
  uptime_secs: number;
  agent?: string;
  memory_pct?: number;
}

/** A hook/telemetry event from the daemon ring buffer or SSE stream. */
export interface HookEvent {
  event_type: string;
  session_id?: string;
  timestamp: number | string;
  [key: string]: unknown;
}

/** Daemon connection state, mirrored by the header health dot. */
export type DaemonHealth = 'ok' | 'connecting' | 'error';

/**
 * A single turn in the coordinator chat transcript.
 *
 * `routed_to` is set when the user prefixed `@session-name:` and the
 * coordinator dispatched the message to that session's tmux pane;
 * `command_output` then carries whatever that pane emitted.
 */
export interface ChatMessage {
  role: 'user' | 'coordinator';
  content: string;
  routed_to?: string;
  command_output?: string;
  timestamp: Date;
}

/** All sessions known to the daemon (polled). */
export const sessions = writable<Session[]>([]);

/** The session whose detail panel is shown; `null` shows the global feed. */
export const activeSessionId = writable<string | null>(null);

/** Current daemon reachability. */
export const daemonHealth = writable<DaemonHealth>('connecting');

/** Recent hook events, newest first (capped). */
export const events = writable<HookEvent[]>([]);

/** Max events retained in the in-memory buffer. */
const EVENT_CAP = 200;

/** The coordinator chat transcript — the GUI's permanent main panel. */
export const chatHistory = writable<ChatMessage[]>([]);

/** Latest coordinator context snapshot (active sessions etc.), or null. */
export const coordinatorContext = writable<any>(null);

/** Whether the left sidebar is shown; toggled from the header/sidebar. */
export const sidebarVisible = writable<boolean>(true);

/** Which sidebar pane is active. */
export const sidebarTab = writable<'sessions' | 'files'>('sessions');

/**
 * Why: The sidebar must reflect daemon state without each row polling on its
 * own; one poller keeps a single timer and updates health as a side effect.
 * What: Calls `list_sessions`, normalizes the payload to `Session[]`, and sets
 * `daemonHealth` to `'ok'` or `'error'` based on the outcome.
 * Test: With the daemon serving two sessions, assert `sessions` length is 2;
 * stop the daemon and assert `daemonHealth` flips to `'error'`.
 */
export async function refreshSessions(): Promise<void> {
  try {
    const raw = await invoke('list_sessions');
    const list: Session[] = Array.isArray(raw)
      ? raw
      : Array.isArray((raw as { sessions?: Session[] })?.sessions)
        ? (raw as { sessions: Session[] }).sessions
        : [];
    sessions.set(list);
    daemonHealth.set('ok');
  } catch {
    daemonHealth.set('error');
  }
}

/**
 * Why: The EventFeed appends live SSE events; capping prevents unbounded
 * memory growth during long-running sessions.
 * What: Prepends `event` to the `events` store and truncates to `EVENT_CAP`.
 * Test: Push `EVENT_CAP + 10` events and assert the store length equals
 * `EVENT_CAP` and the first element is the most recent push.
 */
export function pushEvent(event: HookEvent): void {
  events.update((list) => [event, ...list].slice(0, EVENT_CAP));
}
