import type { AnalyticsProps } from './index'

export { track } from './index'
export type { AnalyticsProps, BeforeSend, BeforeSendEvent } from './index'

/** Loads the script once, and counts a page view for each page with its route template (`/blog/[slug]`). */
export declare function Analytics(props: AnalyticsProps): null
export default Analytics
