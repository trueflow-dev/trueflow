import { retryingFetchReviewSummary, type ReviewRecord } from "./api";

interface ViewModel {
  title: string;
  reviewed: number;
  pending: number;
  records: ReviewRecord[];
}

function summarize(records: ReviewRecord[]): ViewModel {
  const reviewed = records.filter((record) => record.status !== "unreviewed").length;
  return {
    title: "Review Bench Workspace",
    reviewed,
    pending: records.length - reviewed,
    records,
  };
}

function renderRecord(record: ReviewRecord): string {
  const note = record.note ? ` — ${record.note}` : "";
  return `${record.path}: ${record.status}${note}`;
}

export async function renderDashboard(root: HTMLElement): Promise<void> {
  const records = await retryingFetchReviewSummary({ retries: 2 });
  const model = summarize(records);

  root.innerHTML = [
    `<h1>${model.title}</h1>`,
    `<p>Reviewed: ${model.reviewed}</p>`,
    `<p>Pending: ${model.pending}</p>`,
    "<ul>",
    ...model.records.map((record) => `<li>${renderRecord(record)}</li>`),
    "</ul>",
  ].join("\n");
}
