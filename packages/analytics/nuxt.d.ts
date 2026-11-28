import type { AnalyticsProps } from './index'

/** The Nuxt module: add '@forkrail/analytics/nuxt' to `modules`; options go under `analytics`. */
declare const module: any
export default module
export type ModuleOptions = Omit<AnalyticsProps, 'beforeSend' | 'framework'>
