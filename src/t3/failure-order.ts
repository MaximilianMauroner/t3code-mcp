import type { FailureEvidenceOrder } from "./types.js";

export function failureOrderWithEra(order: FailureEvidenceOrder, existing?: FailureEvidenceOrder): FailureEvidenceOrder {
  if (!existing || order.readStartedAt === undefined) return order;
  const boundary = existing.protocolStartedAt ?? 0;
  if (order.readStartedAt < boundary) return order;
  const protocolStartedAt = order.protocolVersion === existing.protocolVersion
    ? boundary
    : order.readStartedAt > (existing.readStartedAt ?? 0) ? order.readStartedAt : 0;
  return protocolStartedAt ? { ...order, protocolStartedAt } : order;
}

export function compareFailureOrder(candidate: FailureEvidenceOrder, existing: FailureEvidenceOrder): number {
  // Admission before a stored protocol transition belongs to an older era,
  // even when the protocol number has returned to the same value.
  if (candidate.readStartedAt !== undefined && candidate.readStartedAt < (existing.protocolStartedAt ?? 0)) return -1;
  if (existing.readStartedAt !== undefined && existing.readStartedAt < (candidate.protocolStartedAt ?? 0)) return 1;
  if (candidate.protocolVersion !== existing.protocolVersion) {
    return (candidate.readStartedAt ?? 0) - (existing.readStartedAt ?? 0);
  }
  return candidate.snapshotSequence - existing.snapshotSequence ||
    (candidate.updatedAt && existing.updatedAt ? Date.parse(candidate.updatedAt) - Date.parse(existing.updatedAt) : 0) ||
    Number(candidate.scope === "full") - Number(existing.scope === "full") ||
    (candidate.item && existing.item
      ? Date.parse(candidate.item.updatedAt) - Date.parse(existing.item.updatedAt) ||
        candidate.item.ordinal - existing.item.ordinal || (candidate.item.id < existing.item.id ? -1 : candidate.item.id > existing.item.id ? 1 : 0)
      : 0);
}

export function samePersistedFailureOrder(left?: FailureEvidenceOrder, right?: FailureEvidenceOrder): boolean {
  // Equal evidence can advance the in-memory admission watermark without a
  // disk write. A changed protocol boundary remains a persistent change.
  const { readStartedAt: leftAdmission, ...leftEvidence } = left ?? {};
  const { readStartedAt: rightAdmission, ...rightEvidence } = right ?? {};
  return JSON.stringify(leftEvidence) === JSON.stringify(rightEvidence);
}
