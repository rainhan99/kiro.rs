/**
 * 被动分母校准观测的展示行。
 *
 * 这些是**在已见样本上对上游算术的观察**，不是实测或公布的上游上限。因此：
 * - 样本数必须与数值一同出现，三个样本和三百个样本不是一回事；
 * - min/max 跨度必须保留，跨度大说明百分比并非简单比例，此时均值不可当作窗口；
 * - 跨度过大时显式给出警示，而不是让读者自己去比两个数字。
 */
export interface CalibrationObservation {
  model: string
  endpoint: string
  samples: number
  minWindowTokens: number
  maxWindowTokens: number
  meanWindowTokens: number
  lastSeen: string
}

export interface CalibrationRow {
  model: string
  endpoint: string
  samples: number
  /** 形如 "200,000"；跨度大时形如 "180,000 – 220,000"。 */
  window: string
  /** 跨度是否大到不该把均值当窗口用。 */
  unstable: boolean
  /** 样本是否少到不足以据此判断。 */
  thin: boolean
}

/** 低于该样本数，数字只能当作轶事。 */
export const THIN_SAMPLE_THRESHOLD = 20
/** max 比 min 高出该比例以上，视为分母不稳定。 */
export const UNSTABLE_SPREAD_RATIO = 1.05

const number = (value: number) => value.toLocaleString('en-US')

export function calibrationRows(observations: unknown): CalibrationRow[] {
  if (!Array.isArray(observations)) return []
  return observations.flatMap((item) => {
    if (item === null || typeof item !== 'object') return []
    const o = item as Partial<CalibrationObservation>
    const numeric = [o.samples, o.minWindowTokens, o.maxWindowTokens, o.meanWindowTokens]
    if (!numeric.every((v) => typeof v === 'number' && Number.isFinite(v))) return []
    if (typeof o.model !== 'string' || typeof o.endpoint !== 'string') return []

    const min = o.minWindowTokens as number
    const max = o.maxWindowTokens as number
    const unstable = min > 0 && max / min > UNSTABLE_SPREAD_RATIO
    return [{
      model: o.model,
      endpoint: o.endpoint,
      samples: o.samples as number,
      // 分母不稳定时不显示均值：显示均值会让读者以为拿到了一个窗口值。
      window: unstable ? `${number(min)} – ${number(max)}` : number(o.meanWindowTokens as number),
      unstable,
      thin: (o.samples as number) < THIN_SAMPLE_THRESHOLD,
    }]
  })
}
