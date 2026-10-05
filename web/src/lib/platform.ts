import type { Platform } from '@/lib/content';

/**
 * Only what the hero needs to choose a command, and nothing that tells one
 * visitor from another: the platform the browser already hands out in its
 * low-entropy client hints, and the coarse name in the user agent where those
 * do not exist (Firefox, Safari).
 */
export interface PlatformHints {
  /** `navigator.userAgentData.platform`, where the browser has it. */
  platform?: string | undefined;
  /** `navigator.userAgentData.mobile`. */
  mobile?: boolean | undefined;
  userAgent: string;
  /** `navigator.maxTouchPoints`: an iPad asks for the desktop site as a Mac. */
  touchPoints?: number | undefined;
}

/** macOS when nothing fits, which is also what the page shows without JavaScript. */
export function detectPlatform(hints: PlatformHints): Platform {
  if (hints.mobile) return 'mobile';
  switch (hints.platform) {
    case 'Windows':
      return 'windows';
    case 'macOS':
      return 'macos';
    case 'Linux':
    case 'Chrome OS':
      return 'linux';
    case 'Android':
    case 'iOS':
      return 'mobile';
  }
  const ua = hints.userAgent;
  if (/Android|iPhone|iPad|iPod|Mobile/i.test(ua)) return 'mobile';
  if (/Windows/i.test(ua)) return 'windows';
  if (/Macintosh|Mac OS X/i.test(ua)) return (hints.touchPoints ?? 0) > 1 ? 'mobile' : 'macos';
  if (/Linux|X11|CrOS/i.test(ua)) return 'linux';
  return 'macos';
}

/** The browser's own hints; `userAgentData` is missing from TypeScript's DOM types. */
export function browserHints(): PlatformHints {
  const data = (navigator as Navigator & { userAgentData?: { platform?: string; mobile?: boolean } }).userAgentData;
  return {
    platform: data?.platform,
    mobile: data?.mobile,
    userAgent: navigator.userAgent,
    touchPoints: navigator.maxTouchPoints,
  };
}
