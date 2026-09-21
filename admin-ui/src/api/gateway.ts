import { createAdminClient } from './admin-client'
import type {
  BudgetUpdate,
  BudgetsView,
  GatewayConfigView,
  PreviewRequest,
  PreviewView,
  SaveGatewayConfig,
} from '@/types/gateway'

const api = createAdminClient()

export async function getGatewayConfig(): Promise<GatewayConfigView> {
  return (await api.get<GatewayConfigView>('/gateway/config')).data
}

export async function saveGatewayConfig(payload: SaveGatewayConfig): Promise<{ revision: number; invalidatesRoutes: boolean }> {
  return (await api.put<{ revision: number; invalidatesRoutes: boolean }>('/gateway/config', payload)).data
}

export async function previewRoute(payload: PreviewRequest): Promise<PreviewView> {
  return (await api.post<PreviewView>('/gateway/preview', payload)).data
}

export async function getBudgets(keyId: number): Promise<BudgetsView> {
  return (await api.get<BudgetsView>(`/client-keys/${keyId}/budgets`)).data
}

export async function saveBudget(keyId: number, payload: BudgetUpdate) {
  return (await api.put(`/client-keys/${keyId}/budgets`, payload)).data
}

export async function adjustBudget(
  keyId: number,
  payload: { adjustmentId: string; unit: string; direction: 'debit' | 'credit'; amount: string; reason: string },
) {
  return (await api.post(`/client-keys/${keyId}/adjustments`, payload)).data
}

export async function startNewCycle(keyId: number, payload: { operationId: string; unit: string; reason: string }) {
  return (await api.post(`/client-keys/${keyId}/cycles`, payload)).data
}

export async function getLedgerAudit(keyId: number) {
  return (await api.get(`/client-keys/${keyId}/ledger-audit`)).data
}
