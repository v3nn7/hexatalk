import { cronJobs } from "convex/server";
import { internal } from "./_generated/api";

const crons = cronJobs();

// Drop declined friend-request rows older than the retention window.
crons.daily(
  "cleanup declined friend requests",
  { hourUTC: 4, minuteUTC: 15 },
  internal.friends.cleanupStaleDeclined,
);

// Drop expired typing rows (clients that crashed/offlined never send
// typing=false). Runs frequently since the TTL is only 6s.
crons.interval(
  "cleanup expired typing indicators",
  { minutes: 5 },
  internal.typing.cleanupExpired,
);

// Drop presence rows whose user account was deleted.
crons.daily(
  "cleanup orphaned presence rows",
  { hourUTC: 4, minuteUTC: 25 },
  internal.presence.cleanupOrphaned,
);

// Drop expired sessions and e-mail verification codes.
crons.interval(
  "cleanup expired auth artifacts",
  { hours: 1 },
  internal.auth.cleanupExpiredAuthArtifacts,
);

// Delete ended voice links (and their leftover ICE rows) past the
// retention window.
crons.interval(
  "cleanup ended voice links",
  { minutes: 10 },
  internal.voice.cleanupEndedVoiceLinks,
);

export default crons;
