export interface PipelineConfig {
  mode: 'off' | 'audit' | 'enforce'
  /** Kiro 表达不了的内容怎么处理（prefill、未知角色、未知内容块……都归它）。与 mode 正交。 */
  unexpressible: 'drop' | 'refuse'
  /** 入站请求抓取。默认关，且默认只抓形状不抓内容。 */
  capture: { enabled: boolean; maxRequests: number; maxBytes: number; redactText: boolean }
  stripBillingHeader: boolean
  cacheStrategy: 'off' | 'static-prefix'
  agentMode: 'vibe' | 'spec'
  ingressMaxBytes: number
  limits: { bodyBytes: number | null; textFieldBytes: number | null; toolResultBytes: number | null; imageBase64Bytes: number | null }
  artifacts: { enabled: boolean; thresholdBytes: number; maxStoreBytes: number; maxArtifactBytes: number; ttlSecs: number; readBytes: number; maxRounds: number }
  toolResults: { strategy: 'join' | 'lossless-chunks'; chunkBytes: number }
  toolCatalog: { strategy: 'inline' | 'on-demand'; budgetBytes: number }
  chunkedMap: { strategy: 'off' | 'model-invoked'; chunkBytes: number; maxChunks: number }
  admission: 'off' | 'declared-ceiling'
  recovery: 'off' | 'lossless-retry'
  images: { strategy: 'preserve' | 'lossless-tiles'; tileMaxBase64Bytes: number; maxTiles: number; maxPixels: number }
  auditEnabled: boolean
  allowSimulatedCache: boolean
  kiroOnly: boolean
}

export interface RequestPipelineSnapshot {
  source: 'startup'
  runtimeEditable: false
  editable: boolean
  effectiveConfig: PipelineConfig
  savedConfig: PipelineConfig | null
  revision: string | null
  restartRequired: boolean
}

export interface SaveRequestPipeline {
  config: PipelineConfig
  revision: string
}
