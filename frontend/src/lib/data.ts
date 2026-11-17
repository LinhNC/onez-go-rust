import type { ShortLink } from './types';

/**
 * Fixtures for the design-system pages. They mirror the accepted concept pages
 * (design/concepts/shorten-admin/index.html, edit.html) so a browser
 * comparison against design/qa/*.png is a like-for-like comparison.
 */
export const BASE_URL = 'https://short.example';

export const links: ShortLink[] = [
  {
    slug: 'product',
    shortUrl: 'https://short.example/product',
    destination: 'https://example.com/products/spring-release',
    title: 'Product launch',
    status: 'active',
    visits: 12481,
    updated: '2 minutes ago',
    created: 'Jun 12, 2026',
    redirectStatus: '302',
    notes: 'Primary short link for the spring product announcement.',
  },
  {
    slug: 'docs',
    shortUrl: 'https://short.example/docs',
    destination: 'https://docs.example.com/getting-started',
    title: 'Public documentation',
    status: 'active',
    visits: 8206,
    updated: 'Yesterday',
    created: 'May 3, 2026',
    redirectStatus: '302',
  },
  {
    slug: 'august',
    shortUrl: 'https://short.example/august',
    destination: 'https://example.com/campaigns/2026/august',
    title: 'August campaign',
    status: 'expired',
    visits: 2914,
    updated: 'Aug 31, 2026',
    created: 'Jul 20, 2026',
    redirectStatus: '301',
  },
  {
    slug: 'brief',
    shortUrl: 'https://short.example/brief',
    destination: 'https://notion.example.com/project-brief',
    title: 'Client review',
    status: 'disabled',
    visits: 438,
    updated: 'Aug 28, 2026',
    created: 'Feb 9, 2026',
    redirectStatus: '302',
  },
  {
    slug: 'contact',
    shortUrl: 'https://short.example/contact',
    destination: 'https://example.com/contact?source=short',
    title: 'Personal contact page',
    status: 'active',
    visits: 186,
    updated: 'Aug 24, 2026',
    created: 'Jan 15, 2026',
    redirectStatus: '302',
  },
];

/** Rows shown by the cleanup preview (age based on creation date). */
export const cleanupLinks: ShortLink[] = [
  {
    slug: 'q3-brief',
    shortUrl: 'https://short.example/q3-brief',
    destination: 'https://notion.example.com/q3-brief',
    title: 'Q3 planning brief',
    status: 'disabled',
    visits: 22,
    updated: 'Sep 2, 2025',
    created: 'Sep 2, 2025',
  },
  {
    slug: 'webinar-spring',
    shortUrl: 'https://short.example/webinar-spring',
    destination: 'https://meet.example.com/webinar/spring',
    title: 'Spring webinar registration',
    status: 'expired',
    visits: 341,
    updated: 'Jun 18, 2025',
    created: 'Jun 18, 2025',
  },
  {
    slug: 'old-shop',
    shortUrl: 'https://short.example/old-shop',
    destination: 'https://shop.example.com/',
    title: 'Legacy storefront',
    status: 'disabled',
    visits: 1204,
    updated: 'May 4, 2025',
    created: 'May 4, 2025',
  },
  {
    slug: 'press-kit',
    shortUrl: 'https://short.example/press-kit',
    destination: 'https://press.example.com/kit-2025',
    title: 'Press kit 2025',
    status: 'expired',
    visits: 76,
    updated: 'Apr 11, 2025',
    created: 'Apr 11, 2025',
  },
];
