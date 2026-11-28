import type { AllowedPropertyValues } from './index'

/**
 * Send a custom event from a function. Pass the request being answered (or its headers), so the
 * event counts as that visitor's; without it, it counts as one visitor of its own.
 */
export declare function track(
  name: string,
  properties?: Record<string, AllowedPropertyValues>,
  options?: { request?: { headers: unknown; url?: string }; headers?: unknown; flags?: string[] | Record<string, AllowedPropertyValues> },
): Promise<void>

declare const _default: { track: typeof track }
export default _default
