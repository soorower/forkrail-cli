import type { AnalyticsProps } from './index'

export { track } from './index'
export type { AnalyticsProps, BeforeSend, BeforeSendEvent } from './index'

/** Loads the script once; with `route`, counts a page view each time the route or path changes. */
export declare function Analytics(props: AnalyticsProps & { route?: string | null; path?: string }): null
export default Analytics
