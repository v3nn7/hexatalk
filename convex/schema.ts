import { defineSchema, defineTable } from "convex/server";
import { v } from "convex/values";

export default defineSchema({
  users: defineTable({
    username: v.string(),
    displayName: v.string(),
    salt: v.string(),
    passwordHash: v.string(),
    // Platform role: user < moderator < admin < owner (Talkyss hierarchy).
    role: v.optional(
      v.union(
        v.literal("user"),
        v.literal("moderator"),
        v.literal("admin"),
        v.literal("owner"),
      ),
    ),
    banned: v.optional(v.boolean()),
    avatarColor: v.optional(v.string()),
    statusMessage: v.optional(v.string()),
    bio: v.optional(v.string()),
    avatarStorageId: v.optional(v.id("_storage")),
    // Base64 X25519 public key (legacy E2EE / fingerprints).
    publicKey: v.optional(v.string()),
    // When false, messages in any conversation this user is in are not
    // persisted on Convex (live peerseal still works). Default: true.
    storeChatHistory: v.optional(v.boolean()),
    // Privacy: hide online status from others (still heartbeats for self).
    hideOnlineStatus: v.optional(v.boolean()),
    // Privacy: only friends can open DMs with you.
    friendsOnlyDms: v.optional(v.boolean()),
    // Privacy: don't appear in global user search (except exact username).
    discoverable: v.optional(v.boolean()),
    // Who may send friend requests: everyone (default) | mutual_servers | nobody.
    friendRequestPrivacy: v.optional(
      v.union(
        v.literal("everyone"),
        v.literal("mutual_servers"),
        v.literal("nobody"),
      ),
    ),
    // Soft presence preference shown to friends: online | idle | dnd | invisible.
    presenceStatus: v.optional(
      v.union(
        v.literal("online"),
        v.literal("idle"),
        v.literal("dnd"),
        v.literal("invisible"),
      ),
    ),
    // Talkyss bots are users without GUI; marked in the client as *bot.
    isBot: v.optional(v.boolean()),
    // Human owner who created the bot (only for isBot users).
    botOwnerId: v.optional(v.id("users")),
  })
    .index("by_username", ["username"])
    .index("by_botOwner", ["botOwnerId"]),

  sessions: defineTable({
    userId: v.id("users"),
    token: v.string(),
    expiresAt: v.number(),
  })
    .index("by_token", ["token"])
    .index("by_userId", ["userId"]),

  loginAttempts: defineTable({
    username: v.string(),
    failedCount: v.number(),
    lockedUntil: v.optional(v.number()),
  }).index("by_username", ["username"]),

  presence: defineTable({
    userId: v.id("users"),
    lastSeenAt: v.number(),
  }).index("by_userId", ["userId"]),

  friendRequests: defineTable({
    fromUserId: v.id("users"),
    toUserId: v.id("users"),
    status: v.union(
      v.literal("pending"),
      v.literal("accepted"),
      v.literal("declined"),
    ),
    // Optional message from the sender (max 200 chars, enforced in mutation).
    note: v.optional(v.string()),
    // Client-facing timestamp of last send/resend (display + ordering).
    sentAt: v.optional(v.number()),
    // Set when accepted or declined (cooldown after decline uses this).
    respondedAt: v.optional(v.number()),
  })
    .index("by_from_and_to", ["fromUserId", "toUserId"])
    .index("by_to_and_status", ["toUserId", "status"])
    .index("by_from_and_status", ["fromUserId", "status"])
    .index("by_status", ["status"]),

  blocks: defineTable({
    blockerId: v.id("users"),
    blockedId: v.id("users"),
  })
    .index("by_blocker_and_blocked", ["blockerId", "blockedId"])
    .index("by_blocker", ["blockerId"]),

  // Per-owner metadata about a friend (private: nickname, favorite, notes).
  friendMeta: defineTable({
    ownerId: v.id("users"),
    friendId: v.id("users"),
    nickname: v.optional(v.string()),
    favorite: v.optional(v.boolean()),
    privateNote: v.optional(v.string()),
  })
    .index("by_owner_and_friend", ["ownerId", "friendId"])
    .index("by_owner", ["ownerId"]),

  conversations: defineTable({
    kind: v.union(v.literal("direct"), v.literal("group"), v.literal("channel")),
    directKey: v.optional(v.string()),
    name: v.optional(v.string()),
    createdBy: v.id("users"),
    lastMessageAt: v.optional(v.number()),
    serverId: v.optional(v.id("servers")),
    // text (default) | voice — only meaningful for kind="channel".
    channelType: v.optional(v.union(v.literal("text"), v.literal("voice"))),
    // Current group-key epoch for encrypted group/channel messages (TGK1).
    keyEpoch: v.optional(v.number()),
  })
    .index("by_directKey", ["directKey"])
    .index("by_server", ["serverId"]),

  servers: defineTable({
    name: v.string(),
    ownerId: v.id("users"),
    inviteCode: v.string(),
    // Optional square icon (owner uploads).
    iconStorageId: v.optional(v.id("_storage")),
    // Vanity path e.g. "talkyss" — only Talkyss app admins may set this.
    customSlug: v.optional(v.string()),
  })
    .index("by_inviteCode", ["inviteCode"])
    .index("by_customSlug", ["customSlug"]),

  // Named roles with a permission bitfield (see convex/roles.ts).
  serverRoles: defineTable({
    serverId: v.id("servers"),
    name: v.string(),
    color: v.string(),
    position: v.number(),
    permissions: v.number(),
  }).index("by_server", ["serverId"]),

  serverMembers: defineTable({
    serverId: v.id("servers"),
    userId: v.id("users"),
    joinedAt: v.number(),
    // Deprecated: superseded by roleIds (multi-role, Discord-style). Left
    // declared so old documents that still carry it don't fail validation;
    // no code reads or writes it anymore except as a one-time fallback for
    // members assigned a role before this field existed.
    roleId: v.optional(v.id("serverRoles")),
    roleIds: v.optional(v.array(v.id("serverRoles"))),
  })
    .index("by_server", ["serverId"])
    .index("by_user", ["userId"])
    .index("by_server_and_user", ["serverId", "userId"]),

  conversationMembers: defineTable({
    conversationId: v.id("conversations"),
    userId: v.id("users"),
    lastReadAt: v.optional(v.number()),
  })
    .index("by_conversation", ["conversationId"])
    .index("by_user", ["userId"])
    .index("by_conversation_and_user", ["conversationId", "userId"]),

  // Per-user per-conversation history preference. When store=false, messages
  // in this conversation are not written to Convex for anyone.
  chatStorePrefs: defineTable({
    userId: v.id("users"),
    conversationId: v.id("conversations"),
    store: v.boolean(),
  })
    .index("by_user_and_conversation", ["userId", "conversationId"])
    .index("by_conversation", ["conversationId"]),

  typing: defineTable({
    conversationId: v.id("conversations"),
    userId: v.id("users"),
    displayName: v.string(),
    expiresAt: v.number(),
  })
    .index("by_conversation", ["conversationId"])
    .index("by_conversation_and_user", ["conversationId", "userId"]),

  messages: defineTable({
    conversationId: v.id("conversations"),
    authorId: v.id("users"),
    authorName: v.string(),
    authorAvatarColor: v.optional(v.string()),
    body: v.string(),
    kind: v.optional(v.union(v.literal("text"), v.literal("call"))),
    attachmentStorageId: v.optional(v.id("_storage")),
    replyToMessageId: v.optional(v.id("messages")),
    encrypted: v.optional(v.boolean()),
    editedAt: v.optional(v.number()),
    deleted: v.optional(v.boolean()),
    deletedAt: v.optional(v.number()),
    deletedBy: v.optional(v.id("users")),
    // Mention metadata, computed client-side at send time (the client owns
    // the plaintext for E2EE chats, so only it can parse @names). Absent on
    // older messages -- treat as "no mentions".
    mentionUserIds: v.optional(v.array(v.id("users"))),
    mentionEveryone: v.optional(v.boolean()),
    // Pinned messages: optional so old documents validate unchanged.
    pinned: v.optional(v.boolean()),
    pinnedAt: v.optional(v.number()),
    pinnedBy: v.optional(v.id("users")),
  }).index("by_conversation", ["conversationId"]),

  reactions: defineTable({
    messageId: v.id("messages"),
    userId: v.id("users"),
    emoji: v.string(),
    // Optional so pre-existing rows (written before this field) validate.
    createdAt: v.optional(v.number()),
  })
    .index("by_message", ["messageId"])
    .index("by_message_and_user_and_emoji", ["messageId", "userId", "emoji"]),

  calls: defineTable({
    conversationId: v.id("conversations"),
    callerId: v.id("users"),
    calleeId: v.id("users"),
    status: v.union(
      v.literal("ringing"),
      v.literal("active"),
      v.literal("ended"),
      v.literal("declined"),
    ),
    offerSdp: v.string(),
    answerSdp: v.optional(v.string()),
    startedAt: v.number(),
    endedAt: v.optional(v.number()),
  })
    .index("by_conversation", ["conversationId"])
    .index("by_caller_and_status", ["callerId", "status"])
    .index("by_callee_and_status", ["calleeId", "status"]),

  // Trickle ICE: candidates each side discovers after sending its
  // offer/answer, exchanged as they're found instead of waiting for ICE
  // gathering to finish before signaling at all (that upfront wait used to
  // add several seconds of dead air before the other side even saw
  // "incoming call").
  callIceCandidates: defineTable({
    callId: v.id("calls"),
    fromUserId: v.id("users"),
    candidate: v.string(),
  }).index("by_call", ["callId"]),

  // Soft presence in a voice room (server voice channel or group call).
  // Real audio is a full-mesh of WebRTC peer links (see voiceLinks).
  voiceStates: defineTable({
    conversationId: v.id("conversations"),
    userId: v.id("users"),
    displayName: v.string(),
    joinedAt: v.number(),
  })
    .index("by_conversation", ["conversationId"])
    .index("by_conversation_and_user", ["conversationId", "userId"])
    .index("by_user", ["userId"]),

  // One WebRTC peer connection per ordered pair of users in a voice room.
  // Offerer is always the lexicographically smaller userId (stable, no glare).
  voiceLinks: defineTable({
    conversationId: v.id("conversations"),
    offererId: v.id("users"),
    answererId: v.id("users"),
    pairKey: v.string(),
    offerSdp: v.string(),
    answerSdp: v.optional(v.string()),
    status: v.union(
      v.literal("offering"),
      v.literal("active"),
      v.literal("ended"),
    ),
    startedAt: v.number(),
  })
    .index("by_conversation", ["conversationId"])
    .index("by_conversation_and_pair", ["conversationId", "pairKey"])
    .index("by_offerer", ["offererId"])
    .index("by_answerer", ["answererId"]),

  voiceLinkIce: defineTable({
    linkId: v.id("voiceLinks"),
    fromUserId: v.id("users"),
    candidate: v.string(),
  }).index("by_link", ["linkId"]),

  // Per-member sealed copy of a conversation group key (AES-256).
  // Server stores only ciphertext sealed to each member's X25519 public key.
  conversationKeyPackages: defineTable({
    conversationId: v.id("conversations"),
    userId: v.id("users"),
    epoch: v.number(),
    sealedKey: v.string(),
    ephPublicKey: v.string(),
    createdBy: v.id("users"),
    createdAt: v.number(),
  })
    .index("by_conversation_and_user", ["conversationId", "userId"])
    .index("by_conversation", ["conversationId"])
    .index("by_user", ["userId"]),

  peerInvites: defineTable({
    conversationId: v.id("conversations"),
    hostUserId: v.id("users"),
    invitePayload: v.string(),
    expiresAt: v.number(),
  }).index("by_conversation", ["conversationId"]),

  // Mobile / desktop push device tokens (FCM / future APNs).
  pushTokens: defineTable({
    userId: v.id("users"),
    token: v.string(),
    platform: v.union(
      v.literal("android"),
      v.literal("ios"),
      v.literal("desktop"),
      v.literal("web"),
    ),
    updatedAt: v.number(),
  })
    .index("by_user", ["userId"])
    .index("by_token", ["token"]),
});

