import type { AnalyticsProps } from '../index'

/** Loads the script once, and counts each page with its route template (`/blog/[slug]`). */
declare function Analytics(props: Omit<AnalyticsProps, 'beforeSend' | 'framework'>): any
export default Analytics
