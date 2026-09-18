import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  adjustBudget,
  getBudgets,
  getGatewayConfig,
  getLedgerAudit,
  previewRoute,
  saveBudget,
  saveGatewayConfig,
  startNewCycle,
} from '@/api/gateway'
import type { BudgetUpdate } from '@/types/gateway'

const configKey = ['gateway-config']

export function useGatewayConfig() {
  return useQuery({ queryKey: configKey, queryFn: getGatewayConfig, refetchOnWindowFocus: false, retry: false })
}

export function useSaveGatewayConfig() {
  const client = useQueryClient()
  return useMutation({
    mutationFn: saveGatewayConfig,
    // 保存后重新拉取：服务端可能补齐了省略的密钥与默认值，界面要显示实际生效的那一份。
    onSuccess: () => client.invalidateQueries({ queryKey: configKey }),
  })
}

export function usePreviewRoute() {
  return useMutation({ mutationFn: previewRoute })
}

export function useBudgets(keyId: number | null) {
  return useQuery({
    queryKey: ['gateway-budgets', keyId],
    queryFn: () => getBudgets(keyId as number),
    enabled: keyId !== null,
    refetchOnWindowFocus: false,
    retry: false,
  })
}

export function useSaveBudget(keyId: number) {
  const client = useQueryClient()
  return useMutation({
    mutationFn: (payload: BudgetUpdate) => saveBudget(keyId, payload),
    onSuccess: () => client.invalidateQueries({ queryKey: ['gateway-budgets', keyId] }),
  })
}

export function useAdjustBudget(keyId: number) {
  const client = useQueryClient()
  return useMutation({
    mutationFn: (payload: { adjustmentId: string; unit: string; direction: 'debit' | 'credit'; amount: string; reason: string }) =>
      adjustBudget(keyId, payload),
    onSuccess: () => {
      client.invalidateQueries({ queryKey: ['gateway-budgets', keyId] })
      client.invalidateQueries({ queryKey: ['gateway-audit', keyId] })
    },
  })
}

export function useNewCycle(keyId: number) {
  const client = useQueryClient()
  return useMutation({
    mutationFn: (payload: { operationId: string; unit: string; reason: string }) => startNewCycle(keyId, payload),
    onSuccess: () => {
      client.invalidateQueries({ queryKey: ['gateway-budgets', keyId] })
      client.invalidateQueries({ queryKey: ['gateway-audit', keyId] })
    },
  })
}

export function useLedgerAudit(keyId: number | null) {
  return useQuery({
    queryKey: ['gateway-audit', keyId],
    queryFn: () => getLedgerAudit(keyId as number),
    enabled: keyId !== null,
    refetchOnWindowFocus: false,
    retry: false,
  })
}
