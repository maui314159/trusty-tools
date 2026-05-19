<script lang="ts">
  // Why: The left sidebar is a context surface for the coordinator chat —
  // picking a session here marks it active (the coordinator uses it for
  // context), but the chat stays as the main panel. The sidebar is dismissable
  // and has two panes: live sessions and tracked files.
  // What: A tab switcher (Sessions | Files) and a close button in the header,
  // then either session rows (status dot, id, workdir, uptime, memory gauge,
  // pause/resume + stop) or the `<FileTracking />` pane.
  // Test: Seed `sessions` with one running session, click the row → it becomes
  // active; click the Files tab → `<FileTracking />` renders; click [×] →
  // `sidebarVisible` becomes false.
  import { Pause, Play, Square, X } from 'lucide-svelte';
  import { invoke } from '../lib/transport';
  import {
    sessions,
    activeSessionId,
    refreshSessions,
    sidebarVisible,
    sidebarTab,
    type Session,
  } from '../stores/app';
  import MemoryGauge from './MemoryGauge.svelte';
  import FileTracking from './FileTracking.svelte';

  /** Color of the status dot for a given session state. */
  function statusTone(status: Session['status']): string {
    switch (status) {
      case 'running':
        return 'bg-status-running status-pulse';
      case 'paused':
        return 'bg-status-paused status-pulse';
      case 'awaiting_approval':
        return 'bg-status-error status-pulse';
      default:
        return 'bg-status-stopped';
    }
  }

  /** Last path segment of a workdir, for a compact label. */
  function basename(path: string): string {
    const parts = path.replace(/\/+$/, '').split('/');
    return parts[parts.length - 1] || path;
  }

  /** Render uptime seconds as `Hh Mm Ss` (compact). */
  function fmtUptime(secs: number): string {
    const h = Math.floor(secs / 3600);
    const m = Math.floor((secs % 3600) / 60);
    const s = secs % 60;
    if (h > 0) return `${h}h ${m}m`;
    if (m > 0) return `${m}m ${s}s`;
    return `${s}s`;
  }

  /**
   * Select a session as the coordinator's context target.
   *
   * Why: The coordinator chat is always the main panel; selecting a session
   * only marks it active so the coordinator and the Files pane can scope to
   * it — it does NOT replace the chat with a detail view.
   * Test: Click a row → `activeSessionId` updates; the chat panel is unchanged.
   */
  function select(id: string): void {
    activeSessionId.set(id);
  }

  /**
   * Why: Pause/resume buttons must reach the daemon and then reflect the new
   * state without a full reload.
   * What: Invokes the matching command for the session's current status, then
   * re-polls. Stops event propagation so the row click does not also fire.
   * Test: Click on a running session's toggle → `pause_session` invoked; on a
   * paused one → `resume_session`.
   */
  async function toggle(session: Session, ev: MouseEvent): Promise<void> {
    ev.stopPropagation();
    const command =
      session.status === 'paused' ? 'resume_session' : 'pause_session';
    try {
      await invoke(command, { id: session.id });
    } finally {
      await refreshSessions();
    }
  }

  /**
   * Why: A stop button lets the user terminate a session from the list.
   * What: Removes the session via the daemon, then re-polls. The daemon's
   * `DELETE /sessions/{id}` route owns the actual teardown.
   * Test: Click stop → the session disappears from the list after refresh.
   */
  async function stop(session: Session, ev: MouseEvent): Promise<void> {
    ev.stopPropagation();
    try {
      await invoke('stop_session', { id: session.id });
    } catch {
      // stop_session has no Tauri command yet; web mode hits the REST route.
    } finally {
      await refreshSessions();
    }
  }
</script>

<aside
  class="flex h-full w-full flex-col overflow-y-auto bg-trusty-surface-light dark:bg-trusty-surface"
>
  <!-- Sidebar header: tab switcher + close button -->
  <div
    class="flex items-center border-b border-trusty-border-light dark:border-trusty-border"
  >
    <button
      type="button"
      on:click={() => sidebarTab.set('sessions')}
      class={`flex-1 px-3 py-2 text-xs font-medium ${
        $sidebarTab === 'sessions'
          ? 'border-b-2 border-trusty-primary text-trusty-primary'
          : 'opacity-60 hover:opacity-100'
      }`}
    >
      Sessions
    </button>
    <button
      type="button"
      on:click={() => sidebarTab.set('files')}
      class={`flex-1 px-3 py-2 text-xs font-medium ${
        $sidebarTab === 'files'
          ? 'border-b-2 border-trusty-primary text-trusty-primary'
          : 'opacity-60 hover:opacity-100'
      }`}
    >
      Files
    </button>
    <button
      type="button"
      on:click={() => sidebarVisible.set(false)}
      aria-label="Hide sidebar"
      class="px-2 py-2 opacity-60 hover:opacity-100"
    >
      <X size={14} />
    </button>
  </div>

  {#if $sidebarTab === 'files'}
    <FileTracking />
  {:else}
    {#if $sessions.length === 0}
      <p class="px-3 py-4 text-xs opacity-60">No sessions registered.</p>
    {/if}

    {#each $sessions as session (session.id)}
    <button
      type="button"
      on:click={() => select(session.id)}
      class={`flex flex-col gap-1 border-b border-trusty-border-light px-3 py-2 text-left dark:border-trusty-border ${
        $activeSessionId === session.id
          ? 'bg-trusty-primary/10'
          : 'hover:bg-trusty-border-light/60 dark:hover:bg-trusty-border/60'
      }`}
    >
      <div class="flex items-center gap-2">
        <span class={`h-2 w-2 shrink-0 rounded-full ${statusTone(session.status)}`}
        ></span>
        <span class="truncate font-mono text-xs">{session.id}</span>
        <span class="ml-auto shrink-0 text-[10px] opacity-60">
          {fmtUptime(session.uptime_secs)}
        </span>
      </div>

      <span class="truncate text-[11px] opacity-70">
        {basename(session.workdir)}
      </span>

      <MemoryGauge pct={session.memory_pct ?? 0} />

      <div class="mt-1 flex items-center gap-2">
        <button
          type="button"
          on:click={(e) => toggle(session, e)}
          aria-label={session.status === 'paused' ? 'Resume' : 'Pause'}
          class="rounded p-0.5 hover:bg-trusty-border-light dark:hover:bg-trusty-border"
        >
          {#if session.status === 'paused'}
            <Play size={13} />
          {:else}
            <Pause size={13} />
          {/if}
        </button>
        <button
          type="button"
          on:click={(e) => stop(session, e)}
          aria-label="Stop"
          class="rounded p-0.5 hover:bg-trusty-border-light dark:hover:bg-trusty-border"
        >
          <Square size={13} />
        </button>
      </div>
    </button>
    {/each}
  {/if}
</aside>
