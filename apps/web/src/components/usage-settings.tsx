import type { MackUsageTotals } from '@waku/client'
import { MackIcon } from '@/components/mack-icon'
import { useMackUsageTotals } from '@/hooks/use-daemon-data'
import { useI18n, type AppLocale } from '@/lib/i18n'
import { cn } from '@/lib/utils'

export function UsageSettings() {
  const { locale, t } = useI18n()
  const usage = useMackUsageTotals()
  const totals = usage.data

  return (
    <div className="mt-1.5 flex min-h-0 flex-col">
      <div className="flex min-h-8 flex-wrap items-center gap-2.5">
        <div className="min-w-0 flex-1" />
        <button
          aria-label={t(usage.isFetching ? 'usage.mack_loading' : 'usage.rescan')}
          className="grid size-7 place-items-center rounded-[7px] border text-[var(--text-tertiary)] outline-none hover:bg-accent focus-visible:ring-1 focus-visible:ring-ring"
          title={t(usage.isFetching ? 'usage.mack_loading' : 'usage.rescan')}
          type="button"
          onClick={() => void usage.refetch()}
        >
          <MackIcon className={cn('size-3', usage.isFetching && 'motion-safe:animate-spin')} name={usage.isFetching ? 'loaderCircle' : 'rotateCw'} />
        </button>
      </div>

      {usage.error && (
        <div className="mt-2 rounded-[7px] border border-[var(--border)] px-3 py-2 text-[11.5px] text-[var(--text-secondary)]">
          {errorMessage(usage.error)}
        </div>
      )}

      <div className="mt-[18px] flex flex-col gap-2.5">
        <div className="text-[10px] uppercase tracking-[0.02em] text-[var(--text-tertiary)]">
          {t('usage.mack_tokens_upper')}
        </div>
        <div className="text-[30px] font-medium leading-tight tabular-nums">
          {totals ? formatNumber(totalTokens(totals), locale) : '—'}
        </div>
        <div className="text-[10.5px] text-[var(--text-tertiary)]">
          {totals
            ? t('usage.mack_turns_chats', {
              turns: formatCount(totals.turns, locale),
              chats: formatCount(totals.sessions, locale),
            })
            : t('usage.mack_loading')}
        </div>
      </div>

      {totals && <MackUsageTiles totals={totals} />}
    </div>
  )
}

function MackUsageTiles({ totals }: { totals: MackUsageTotals }) {
  const { locale, t } = useI18n()
  const observedInput = totals.totals.uncachedInput + totals.totals.cachedInput
  const cachedShare = observedInput ? totals.totals.cachedInput / observedInput : 0
  const tiles = [
    [t('usage.cached_input'), formatNumber(totals.totals.cachedInput, locale), t('usage.observed_input_share', { share: formatPercent(cachedShare, locale) })],
    [t('usage.uncached_input'), formatNumber(totals.totals.uncachedInput, locale), t('usage.cache_writes', { count: formatNumber(totals.totals.cacheCreation, locale) })],
    [t('usage.output'), formatNumber(totals.totals.output, locale), t('usage.includes_reasoning', { count: formatNumber(totals.totals.reasoning, locale) })],
  ]
  return (
    <div className="mt-6 grid border-y sm:grid-cols-3">
      {tiles.map(([label, value, detail], index) => (
        <div className={cn('min-w-0 px-3.5 py-2.5', index > 0 && 'border-t sm:border-l sm:border-t-0')} key={label}>
          <div className="truncate text-[10px] text-[var(--text-tertiary)]">{label}</div>
          <div className="mt-0.5 truncate text-[15px] tabular-nums">{value}</div>
          <div className="mt-0.5 truncate text-[9.5px] text-[var(--text-tertiary)]">{detail}</div>
        </div>
      ))}
    </div>
  )
}

function totalTokens(totals: MackUsageTotals) {
  return totals.totals.uncachedInput + totals.totals.cachedInput + totals.totals.cacheCreation + totals.totals.output
}

function formatNumber(value: number, locale?: AppLocale) {
  return new Intl.NumberFormat(locale, { notation: value >= 10_000 ? 'compact' : 'standard', maximumFractionDigits: 1 }).format(value)
}

function formatPercent(value: number, locale?: AppLocale) {
  return new Intl.NumberFormat(locale, { style: 'percent', maximumFractionDigits: 1 }).format(value)
}

function formatCount(value: number, locale?: AppLocale) {
  return new Intl.NumberFormat(locale).format(value)
}

function errorMessage(error: unknown) {
  return error instanceof Error ? error.message : String(error)
}
