# PRD v0.2 (English Version – Frozen)

## 1. Background / Motivation
- The existing Nginx-based streaming system has pain points: repository structure is messy and difficult to maintain.
- Decision: migrate to **MediaMTX**, which is actively maintained, well-documented, and easy to understand.
- Goal: switch the backend to MediaMTX with minimal frontend changes, enabling stream management and FFmpeg-based live streaming.

## 2. Scope / Goals
- **Backend responsibilities:**
    - Manage **stream paths**
    - Manage FFmpeg jobs for pushing streams to external platforms (e.g., YouTube, Facebook, Instagram)
    - Provide clear API endpoints for frontend consumption
- Frontend changes are allowed but **not part of this PRD**.
- **Deployment:**
    - No Docker; run **Node.js + MediaMTX** directly
    - Target environment: GCP instance
- **User system:**
    - Single admin account
    - Simple authentication (auth)
- **State management:**
    - Only maintain **active stream paths**
    - Log all user actions (create/update/delete for paths and outputs) for auditing purposes

## 3. Non-Goals / Out of Scope
- Complex frontend UI changes
- High concurrency or high availability design
- Historical stream state maintenance (only logs are kept)
- Multi-user and permission management
- Performance optimization (low-concurrency environment; readability and maintainability prioritized)

## 4. Key Design Assumptions
1. MediaMTX manages paths and monitors publishers/readers but does **not perform encoding/decoding**.
2. One publisher can write to a stream path; multiple readers can read concurrently.
3. The system relies on FFmpeg to push streams to external platforms; the backend manages job lifecycles.
4. Configuration readability is important; JSON/NDJSON storage is sufficient.
5. Deployment environment is simple: Node.js + MediaMTX + FFmpeg.

## 5. Future Considerations / Possible Enhancements
**MVP Out-of-Scope Features**
- Stream recording and download
- Uploading local video files and converting to streams
- Output rotation/filter (for Instagram scenario)
- Protocol mapping (e.g., SRT → RTMP)
- Advanced redundancy: SRT bonding, failover, real-time stream switching

**MVP Potential Enhancements**
- Stream key management (internal path naming)
- Pipeline creation and assignment
- Metrics monitoring (bitrate, codec, resolution, sample rate) using MediaMTX and/or ffprobe

---

### Conclusion / Frozen Points
- This version clearly defines: **backend responsibilities, deployment assumptions, single admin user, active stream path maintenance, and action logging**
- Serves as a solid foundation for **RFC v0.1**
