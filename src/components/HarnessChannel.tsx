/**
 * Which dsh release the managed runtime installs.
 *
 * The build ships one verified release; everything else the registry offers is
 * a promise nobody has checked yet. So a switch is never just a dropdown: the
 * chosen release is probed first (preflight), what it cannot take is said out
 * loud, and only then is the runtime reinstalled. A release whose browser
 * sign-in has moved cannot be taken at all — the frame would meet a 401.
 */
import { useEffect, useState } from 'react'
import { Loader2, RefreshCw, TriangleAlert } from 'lucide-react'

import { Button } from '@/components/Button'
import { t } from '@/lib/i18n'
import {
  dshChannel,
  dshChannelPin,
  dshChannelRefresh,
  dshPreflight,
  type ChannelStatus,
} from '@/lib/ipc'
import { useHarness } from '@/state/harness'

export function HarnessChannelControl() {
  const [channel, setChannel] = useState<ChannelStatus | null>(null)
  const [choice, setChoice] = useState<string | null>(null)
  const [refreshing, setRefreshing] = useState(false)
  const [checking, setChecking] = useState(false)
  const [degraded, setDegraded] = useState<string | null>(null)
  const [problem, setProblem] = useState<string | null>(null)
  const install = useHarness((state) => state.install)
  const installing = useHarness((state) => state.installing)

  useEffect(() => {
    dshChannel()
      .then(setChannel)
      .catch(() => setProblem(t('dsh.channel.loadFailed')))
  }, [])

  const current = channel ? (channel.pinned ?? channel.builtin) : ''
  const selected = channel ? (choice ?? current) : ''
  const dirty = channel !== null && selected !== current
  const pending =
    channel?.pinned && channel.pinned !== channel.installed ? channel.pinned : null

  const refresh = async () => {
    setRefreshing(true)
    setProblem(null)
    try {
      setChannel(await dshChannelRefresh())
    } catch (cause) {
      setProblem(String(cause))
    } finally {
      setRefreshing(false)
    }
  }

  const apply = async (version: string) => {
    setChecking(true)
    setProblem(null)
    setDegraded(null)
    try {
      const report = await dshPreflight(version)
      if (!report.connection) {
        setProblem(t('dsh.channel.unsupported', { version }))
        return
      }
      // The picker enhancement is optional: missing it downgrades a feature,
      // never the frame. Said once, then the choice is the user's.
      if (!report.picker && degraded !== version) {
        setDegraded(version)
        return
      }
      setChannel(await dshChannelPin(version === channel?.builtin ? null : version))
      setChoice(null)
      await install()
      setChannel(await dshChannel())
    } catch (cause) {
      setProblem(String(cause))
    } finally {
      setChecking(false)
    }
  }

  const busy = refreshing || checking || installing
  const versions = channel?.known ?? []

  return (
    <div className="flex flex-col items-end gap-2">
      <div className="flex items-center gap-2">
        <select
          value={selected}
          disabled={channel === null || busy}
          aria-label={t('dsh.channel')}
          onChange={(event) => {
            setChoice(event.target.value)
            setDegraded(null)
            setProblem(null)
          }}
          className="h-[30px] max-w-[180px] rounded-control border border-line-strong bg-surface-2 px-2.5 text-[11.5px] text-text outline-none focus:border-brand"
        >
          {channel !== null && !versions.includes(channel.builtin) && (
            <option value={channel.builtin}>
              {t('dsh.channel.builtin', { version: channel.builtin })}
            </option>
          )}
          {versions.map((version) => (
            <option key={version} value={version}>
              {version === channel?.builtin
                ? t('dsh.channel.builtin', { version })
                : version}
            </option>
          ))}
          {versions.length === 0 && channel !== null && (
            <option value={current}>{channel.selected}</option>
          )}
        </select>
        <button
          type="button"
          onClick={() => void refresh()}
          disabled={busy}
          title={t('dsh.channel.refresh')}
          aria-label={t('dsh.channel.refresh')}
          className="flex h-[30px] w-[30px] items-center justify-center rounded-control border border-line-strong bg-surface-2 text-faint outline-none transition-colors duration-100 enabled:hover:text-text focus:border-brand disabled:opacity-50"
        >
          <RefreshCw size={13} strokeWidth={2.2} className={refreshing ? 'animate-spin' : ''} />
        </button>
        {dirty && (
          <Button variant="secondary" disabled={busy} onClick={() => void apply(selected)}>
            {checking ? <Loader2 size={12} className="animate-spin" /> : null}
            {t('dsh.channel.switch')}
          </Button>
        )}
      </div>
      {pending !== null && (
        <span className="text-[11px] text-warn">
          {t('dsh.channel.pending', {
            version: pending,
            installed: channel?.installed ?? t('check.harness.unknown'),
          })}
        </span>
      )}
      {channel?.updateAvailable && channel.updateAvailable !== channel.installed && (
        <span className="text-[11px] text-brand">
          {t('dsh.channel.update', { version: channel.updateAvailable })}
        </span>
      )}
      {degraded !== null && (
        <span className="flex max-w-[320px] items-start gap-1.5 text-right text-[11px] leading-relaxed text-warn">
          <TriangleAlert size={12} strokeWidth={2.2} className="mt-[2px] shrink-0" />
          {t('dsh.channel.degraded')}
          <button
            type="button"
            className="shrink-0 font-medium underline underline-offset-2"
            onClick={() => void apply(degraded)}
          >
            {t('dsh.channel.degradedGo')}
          </button>
        </span>
      )}
      {problem !== null && (
        <span className="flex max-w-[320px] items-start gap-1.5 text-right text-[11px] leading-relaxed text-danger">
          <TriangleAlert size={12} strokeWidth={2.2} className="mt-[2px] shrink-0" />
          {problem}
        </span>
      )}
    </div>
  )
}
