// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

/** @ignore */

import type {
  invoke,
  transformCallback,
  convertFileSrc,
  InvokeOptions
} from './core'

/** @ignore */
declare global {
  interface Window {
    __TAURI_INTERNALS__: {
      invoke: typeof invoke
      transformCallback: typeof transformCallback
      unregisterCallback: (id: number) => void
      runCallback: (id: number, data: unknown) => void
      runCallbackWithJson: (id: number, data: string) => void
      parseJson: (callbackId: number, data: string) => unknown
      callbacks: Map<number, (data: unknown) => void>
      convertFileSrc: typeof convertFileSrc
      ipc: (message: {
        cmd: string
        callback: number
        error: number
        payload: unknown
        options?: InvokeOptions & {
          responseNeedsJsonParse?: boolean
        }
      }) => void
      metadata: {
        currentWindow: WindowDef
        currentWebview: WebviewDef
      }
      plugins: {
        path: {
          sep: string
          delimiter: string
        }
      }
    }
    __TAURI_EVENT_PLUGIN_INTERNALS__: {
      unregisterListener: (event: string, eventId: number) => void
    }
  }
}

/** @ignore */
interface WebviewDef {
  windowLabel: string
  label: string
}

/** @ignore */
interface WindowDef {
  label: string
}
