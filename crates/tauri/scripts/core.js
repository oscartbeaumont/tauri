// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

;(function () {
  const RAW_JSON_KEY = '__TAURI_JSON_STRING__'

  function uid() {
    return window.crypto.getRandomValues(new Uint32Array(1))[0]
  }

  const osName = __TEMPLATE_os_name__
  const protocolScheme = __TEMPLATE_protocol_scheme__

  Object.defineProperty(window.__TAURI_INTERNALS__, 'convertFileSrc', {
    value: function (filePath, protocol = 'asset') {
      const path = encodeURIComponent(filePath)
      return osName === 'windows' || osName === 'android'
        ? `${protocolScheme}://${protocol}.localhost/${path}`
        : `${protocol}://localhost/${path}`
    }
  })

  const callbacks = new Map()
  const invokeMetadata = new Map()

  function registerCallback(callback, once) {
    const identifier = uid()
    callbacks.set(identifier, (data) => {
      if (once) {
        unregisterCallback(identifier)
      }
      return callback && callback(data)
    })
    return identifier
  }

  function unregisterCallback(id) {
    callbacks.delete(id)
    invokeMetadata.delete(id)
  }

  function runCallback(id, data) {
    const callback = callbacks.get(id)
    if (callback) {
      try {
        const metadata = invokeMetadata.get(id)

        if (
          metadata
          && typeof metadata.reviver === 'function'
          && data
          && typeof data === 'object'
          && RAW_JSON_KEY in data
        ) {
          callback(JSON.parse(data[RAW_JSON_KEY], metadata.reviver))
        } else {
          callback(data)
        }
      } catch (error) {
        const metadata = invokeMetadata.get(id)
        const target = metadata?.error ?? id
        const targetCallback = callbacks.get(target)

        if (targetCallback) {
          targetCallback(error instanceof Error ? error.message : `${error}`)
        } else {
          throw error
        }
      }
    } else {
      console.warn(
        `[TAURI] Couldn't find callback id ${id}. This might happen when the app is reloaded while Rust is running an asynchronous operation.`
      )
    }
  }

  // Maybe let's rename it to `registerCallback`?
  Object.defineProperty(window.__TAURI_INTERNALS__, 'transformCallback', {
    value: registerCallback
  })

  Object.defineProperty(window.__TAURI_INTERNALS__, 'unregisterCallback', {
    value: unregisterCallback
  })

  Object.defineProperty(window.__TAURI_INTERNALS__, 'runCallback', {
    value: runCallback
  })

  // This is just for the debugging purposes
  Object.defineProperty(window.__TAURI_INTERNALS__, 'callbacks', {
    value: callbacks
  })

  const ipcQueue = []
  let isWaitingForIpc = false

  function waitForIpc() {
    if ('ipc' in window.__TAURI_INTERNALS__) {
      for (const action of ipcQueue) {
        action()
      }
    } else {
      setTimeout(waitForIpc, 50)
    }
  }

  Object.defineProperty(window.__TAURI_INTERNALS__, 'invoke', {
    value: function (cmd, payload = {}, options) {
      return new Promise(function (resolve, reject) {
        const ipcOptions = options ? { ...options } : {}
        const reviver = ipcOptions?.reviver
        const shouldUseReviver = typeof reviver === 'function'

        if (ipcOptions && 'reviver' in ipcOptions) {
          delete ipcOptions.reviver
        }

        const callback = registerCallback((r) => {
          resolve(r)
          unregisterCallback(error)
        }, true)
        const error = registerCallback((e) => {
          reject(e)
          unregisterCallback(callback)
        }, true)

        if (shouldUseReviver) {
          invokeMetadata.set(callback, { reviver, error })
          invokeMetadata.set(error, { reviver, error })

          ipcOptions.responseNeedsJsonParse = true
        }

        const action = () => {
          window.__TAURI_INTERNALS__.ipc({
            cmd,
            callback,
            error,
            payload,
            options: Object.keys(ipcOptions).length > 0 ? ipcOptions : undefined
          })
        }
        if ('ipc' in window.__TAURI_INTERNALS__) {
          action()
        } else {
          ipcQueue.push(action)
          if (!isWaitingForIpc) {
            waitForIpc()
            isWaitingForIpc = true
          }
        }
      })
    }
  })
})()
