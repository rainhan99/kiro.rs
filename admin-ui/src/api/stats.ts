import { createAdminClient } from './admin-client'
import type {
  CredentialDistribution,
  KeyDistribution,
  ModelDistribution,
  OverviewStats,
  StatsFilter,
  StatsTimeFilter,
  TimeSeriesPoint,
} from '@/types/api'

const api = createAdminClient()

export async function getOverview(): Promise<OverviewStats> {
  const { data } = await api.get<OverviewStats>('/stats/overview')
  return data
}

function statsParams(time: StatsTimeFilter, filter?: StatsFilter) {
  return {
    ...time,
    ...(filter?.keyId !== undefined ? { keyId: filter.keyId } : {}),
    ...(filter?.group ? { group: filter.group } : {}),
  }
}

export async function getTimeSeries(time: StatsTimeFilter, filter?: StatsFilter): Promise<TimeSeriesPoint[]> {
  const { data } = await api.get<TimeSeriesPoint[]>('/stats/timeseries', {
    params: statsParams(time, filter),
  })
  return data
}

export async function getByModel(time: StatsTimeFilter, filter?: StatsFilter): Promise<ModelDistribution[]> {
  const { data } = await api.get<ModelDistribution[]>('/stats/by-model', {
    params: statsParams(time, filter),
  })
  return data
}

export async function getByCredential(time: StatsTimeFilter, filter?: StatsFilter): Promise<CredentialDistribution[]> {
  const { data } = await api.get<CredentialDistribution[]>('/stats/by-credential', {
    params: statsParams(time, filter),
  })
  return data
}

export async function getByKey(time: StatsTimeFilter, filter?: StatsFilter): Promise<KeyDistribution[]> {
  // by-key 是横向对比所有 Key，忽略 keyId 过滤；仅透传时间窗与分组
  const { data } = await api.get<KeyDistribution[]>('/stats/by-key', {
    params: { ...time, ...(filter?.group ? { group: filter.group } : {}) },
  })
  return data
}
