import type { AnalyticsProps } from './index'

export { track } from './index'
export type { AnalyticsProps, BeforeSend, BeforeSendEvent } from './index'

/** Loads the script once; page views follow the address, client-side navigations included. */
export declare function Analytics(props: AnalyticsProps): null
export default Analytics
