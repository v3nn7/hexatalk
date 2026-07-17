import { cronJobs } from "convex/server";
import { internal } from "./_generated/api";

const crons = cronJobs();

// Drop declined friend-request rows older than the retention window.
crons.daily(
  "cleanup declined friend requests",
  { hourUTC: 4, minuteUTC: 15 },
  internal.friends.cleanupStaleDeclined,
);

export default crons;
