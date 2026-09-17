export interface PipelineConfig {
  mode: 'off' | 'audit' | 'enforce'
  stripBillingHeader: boolean
  cacheStrategy: 'off' | 'static-prefix'
  agentMode: 'vibe' | 'spec'
  ingressMaxBytes: number
  limits: { bodyBytes: number | null; textFieldBytes: number | null; toolResultBytes: number | null; imageBase64Bytes: number | null }
  artifacts: { enabled: boolean; thresholdBytes: number; maxStoreBytes: number; maxArtifactBytes: number; ttlSecs: number; readBytes: number; maxRounds: number }
  toolResults: { strategy: 'join' | 'lossless-chunks'; chunkBytes: number }
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
