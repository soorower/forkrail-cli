import type { AnalyticsProps } from './index'

export { track } from './index'
export type { AnalyticsProps, BeforeSend, BeforeSendEvent } from './index'

/** Loads the script once, in the browser; page views follow the address. */
export declare function injectAnalytics(props?: AnalyticsProps): void
