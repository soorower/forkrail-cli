import type { DefineComponent } from 'vue'
import type { AnalyticsProps } from './index'

export { track } from './index'
export type { AnalyticsProps, BeforeSend, BeforeSendEvent } from './index'

/** Loads the script once; page views follow the address, client-side navigations included. */
export declare const Analytics: DefineComponent<AnalyticsProps>
export default Analytics
