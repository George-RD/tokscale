import {
  deriveClientBreakdownProvenance,
  type ClientBreakdownData,
  type ModelBreakdownData,
} from "./helpers";

export const SUPPORTED_VERSIONED_PARSERS: Readonly<Record<string, number>> = {
  copilot: 2,
};

const TOKEN_FIELDS = [
  "input",
  "output",
  "cacheRead",
  "cacheWrite",
  "reasoning",
] as const;
type TokenField = (typeof TOKEN_FIELDS)[number];

export interface ParserAggregateHighWater {
  tokens: number;
  input: number;
  output: number;
  cacheRead: number;
  cacheWrite: number;
  reasoning: number;
  messages: number;
}

export interface ParserClientHighWaterState {
  version: number;
  aggregate: ParserAggregateHighWater;
  /** Cellwise maxima for attribution of bounded aggregate growth. */
  days: Record<string, ClientBreakdownData>;
}

export type DeviceParserStates = Record<string, ParserClientHighWaterState>;

export interface IncomingParserContribution {
  date: string;
  clients: Array<{
    client: string;
    modelId: string;
    tokens: {
      input: number;
      output: number;
      cacheRead: number;
      cacheWrite: number;
      reasoning?: number;
    };
    cost: number;
    messages: number;
  }>;
}

export type ParserHighWaterMode =
  | "status-quo"
  | "freeze"
  | "baseline-legacy"
  | "baseline-new"
  | "incremental";

export interface ParserHighWaterPlan {
  mode: ParserHighWaterMode;
  increments: Record<string, ClientBreakdownData>;
  nextState?: ParserClientHighWaterState;
}

function dictionary<T>(): Record<string, T> {
  return Object.create(null) as Record<string, T>;
}

function copyDictionary<T>(source?: Record<string, T>): Record<string, T> {
  return Object.assign(dictionary<T>(), source);
}

function ownValue<T>(
  source: Record<string, T> | undefined,
  key: string
): T | undefined {
  return source && Object.prototype.hasOwnProperty.call(source, key)
    ? source[key]
    : undefined;
}

function copyModels(
  source?: Record<string, ModelBreakdownData>
): Record<string, ModelBreakdownData> {
  const result = dictionary<ModelBreakdownData>();
  for (const [modelId, model] of Object.entries(source ?? {})) {
    result[modelId] = { ...model };
  }
  return result;
}

function emptyAggregate(): ParserAggregateHighWater {
  return {
    tokens: 0,
    input: 0,
    output: 0,
    cacheRead: 0,
    cacheWrite: 0,
    reasoning: 0,
    messages: 0,
  };
}

function emptyModel(): ModelBreakdownData {
  return {
    tokens: 0,
    cost: 0,
    input: 0,
    output: 0,
    cacheRead: 0,
    cacheWrite: 0,
    reasoning: 0,
    messages: 0,
  };
}

function modelFromContribution(
  contribution: IncomingParserContribution["clients"][number]
): ModelBreakdownData {
  const input = contribution.tokens.input;
  const output = contribution.tokens.output;
  const cacheRead = contribution.tokens.cacheRead;
  const cacheWrite = contribution.tokens.cacheWrite;
  const reasoning = contribution.tokens.reasoning ?? 0;
  return {
    tokens: input + output + cacheRead + cacheWrite + reasoning,
    cost: contribution.cost,
    input,
    output,
    cacheRead,
    cacheWrite,
    reasoning,
    messages: contribution.messages,
  };
}

function breakdownFromModels(
  models: Record<string, ModelBreakdownData>
): ClientBreakdownData {
  const breakdown: ClientBreakdownData = {
    ...emptyModel(),
    models,
  };
  for (const model of Object.values(models)) {
    model.cost = quantizeCost(model.cost);
    breakdown.tokens += model.tokens;
    breakdown.cost += model.cost;
    breakdown.input += model.input;
    breakdown.output += model.output;
    breakdown.cacheRead += model.cacheRead;
    breakdown.cacheWrite += model.cacheWrite;
    breakdown.reasoning += model.reasoning;
    breakdown.messages += model.messages;
  }
  breakdown.cost = quantizeCost(breakdown.cost);
  breakdown.provenance = deriveClientBreakdownProvenance(breakdown);
  return breakdown;
}

export function foldParserClientSnapshot(
  contributions: IncomingParserContribution[],
  client: string
): Record<string, ClientBreakdownData> {
  const modelsByDay = new Map<string, Record<string, ModelBreakdownData>>();
  for (const day of contributions) {
    for (const contribution of day.clients) {
      if (contribution.client !== client) continue;
      const models = modelsByDay.get(day.date) ?? dictionary<ModelBreakdownData>();
      const incoming = modelFromContribution(contribution);
      const model = ownValue(models, contribution.modelId) ?? emptyModel();
      for (const field of TOKEN_FIELDS) model[field] += incoming[field];
      model.tokens = TOKEN_FIELDS.reduce((sum, field) => sum + model[field], 0);
      model.cost += incoming.cost;
      model.messages += incoming.messages;
      models[contribution.modelId] = model;
      modelsByDay.set(day.date, models);
    }
  }

  const days = dictionary<ClientBreakdownData>();
  for (const [date, models] of modelsByDay) {
    days[date] = breakdownFromModels(models);
  }
  return days;
}

function aggregateSnapshot(
  days: Record<string, ClientBreakdownData>
): ParserAggregateHighWater {
  const aggregate = emptyAggregate();
  for (const day of Object.values(days)) {
    for (const field of TOKEN_FIELDS) aggregate[field] += day[field] || 0;
    aggregate.tokens += day.tokens || 0;
    aggregate.messages += day.messages || 0;
  }
  return aggregate;
}

function maxModel(
  previous: ModelBreakdownData | undefined,
  incoming: ModelBreakdownData
): ModelBreakdownData {
  const result = emptyModel();
  let tokenHighWaterAdvanced = previous == null;
  for (const field of TOKEN_FIELDS) {
    const prior = previous?.[field] ?? 0;
    const next = incoming[field] || 0;
    result[field] = Math.max(prior, next);
    tokenHighWaterAdvanced ||= next > prior;
  }
  result.tokens = TOKEN_FIELDS.reduce((sum, field) => sum + result[field], 0);
  // Cost is contextual data for the token high-water snapshot, never its own
  // monotonic signal: repricing alone must not authorize spend.
  result.cost = quantizeCost(
    tokenHighWaterAdvanced ? positive(incoming.cost) : previous?.cost ?? 0
  );
  result.messages = Math.max(
    positive(previous?.messages ?? 0),
    positive(incoming.messages)
  );
  return result;
}

function advanceDays(
  previous: Record<string, ClientBreakdownData>,
  incoming: Record<string, ClientBreakdownData>
): Record<string, ClientBreakdownData> {
  const next = copyDictionary(previous);
  for (const [date, day] of Object.entries(incoming)) {
    const prior = ownValue(previous, date);
    const models = copyDictionary(prior?.models);
    for (const [modelId, model] of Object.entries(day.models)) {
      models[modelId] = maxModel(ownValue(prior?.models, modelId), model);
    }
    next[date] = breakdownFromModels(models);
  }
  return next;
}

function advanceAggregate(
  previous: ParserAggregateHighWater,
  incoming: ParserAggregateHighWater
): ParserAggregateHighWater {
  return {
    tokens: Math.max(previous.tokens, incoming.tokens),
    input: Math.max(previous.input, incoming.input),
    output: Math.max(previous.output, incoming.output),
    cacheRead: Math.max(previous.cacheRead, incoming.cacheRead),
    cacheWrite: Math.max(previous.cacheWrite, incoming.cacheWrite),
    reasoning: Math.max(previous.reasoning, incoming.reasoning),
    messages: Math.max(previous.messages, incoming.messages),
  };
}

const COST_SCALE = 10_000;

function quantizeCost(value: number): number {
  return Math.round((positive(value) + Number.EPSILON) * COST_SCALE) / COST_SCALE;
}

function positive(value: number): number {
  return Number.isFinite(value) ? Math.max(0, value) : 0;
}

function allocateIncrements(
  previousDays: Record<string, ClientBreakdownData>,
  incomingDays: Record<string, ClientBreakdownData>,
  previousAggregate: ParserAggregateHighWater,
  incomingAggregate: ParserAggregateHighWater
): Record<string, ClientBreakdownData> {
  let tokenBudget = positive(incomingAggregate.tokens - previousAggregate.tokens);
  let messageBudget = positive(
    incomingAggregate.messages - previousAggregate.messages
  );
  const bucketBudgets: Record<TokenField, number> = {
    input: positive(incomingAggregate.input - previousAggregate.input),
    output: positive(incomingAggregate.output - previousAggregate.output),
    cacheRead: positive(incomingAggregate.cacheRead - previousAggregate.cacheRead),
    cacheWrite: positive(incomingAggregate.cacheWrite - previousAggregate.cacheWrite),
    reasoning: positive(incomingAggregate.reasoning - previousAggregate.reasoning),
  };
  const increments = dictionary<ClientBreakdownData>();
  const dates = Object.keys(incomingDays).sort((a, b) => b.localeCompare(a));
  for (const date of dates) {
    const incrementModels = dictionary<ModelBreakdownData>();
    const incoming = incomingDays[date];
    const previous = ownValue(previousDays, date);
    for (const modelId of Object.keys(incoming.models).sort()) {
      const model = incoming.models[modelId];
      const prior = ownValue(previous?.models, modelId);
      const increment = emptyModel();
      let candidateTokens = 0;
      for (const field of TOKEN_FIELDS) {
        const candidate = positive(model[field] - (prior?.[field] ?? 0));
        candidateTokens += candidate;
        const accepted = Math.min(candidate, bucketBudgets[field], tokenBudget);
        increment[field] = accepted;
        bucketBudgets[field] -= accepted;
        tokenBudget -= accepted;
      }
      increment.tokens = TOKEN_FIELDS.reduce(
        (sum, field) => sum + increment[field],
        0
      );

      // Metadata is cumulative in a date/model cell, so only its marginal
      // growth can accompany new usage. If moves make cell growth exceed the
      // device-wide token budget, the deterministic date/model ordering above
      // receives the same fraction of marginal cost; the rest is deliberately
      // not credited because no safe cell attribution exists. Repricing alone
      // still cannot authorize spend.
      if (increment.tokens > 0) {
        const acceptedFraction =
          candidateTokens > 0 ? increment.tokens / candidateTokens : 0;
        const marginalCost = positive(model.cost - (prior?.cost ?? 0));
        increment.cost = quantizeCost(marginalCost * acceptedFraction);
      }

      // Counts have their own device/client lifetime high-water. That lets a
      // one-token new session add one whole message without using a lossy
      // token fraction, while pure date/model moves have zero count budget.
      const candidateMessages = positive(
        model.messages - (prior?.messages ?? 0)
      );
      increment.messages = Math.min(candidateMessages, messageBudget);
      messageBudget -= increment.messages;

      if (increment.tokens > 0 || increment.messages > 0) {
        incrementModels[modelId] = increment;
      }
    }
    if (Object.keys(incrementModels).length > 0) {
      increments[date] = breakdownFromModels(incrementModels);
    }
  }
  return increments;
}

function validState(
  state: ParserClientHighWaterState | undefined,
  version: number
): state is ParserClientHighWaterState {
  return Boolean(
    state &&
      state.version === version &&
      state.aggregate &&
      state.days &&
      Number.isFinite(state.aggregate.tokens) &&
      state.aggregate.tokens >= 0 &&
      TOKEN_FIELDS.every(
        (field) =>
          Number.isFinite(state.aggregate[field]) &&
          state.aggregate[field] >= 0
      ) &&
      Number.isFinite(state.aggregate.messages) &&
      state.aggregate.messages >= 0
  );
}

/**
 * Build a non-destructive parser rollout plan.
 *
 * The first supported full snapshot becomes a baseline. If legacy rows exist,
 * that snapshot is deliberately not added. Later full snapshots may add only
 * growth bounded BOTH by positive per-cell deltas and by device/client-wide
 * cumulative high-water growth. Date/model reshuffles and deleted local
 * history therefore add nothing and can never erase or duplicate stored rows.
 */
export function planParserHighWaterSubmission(args: {
  client: string;
  incomingVersion?: number;
  fullHistory: boolean;
  existingLegacyDays: Record<string, ClientBreakdownData>;
  incomingDays: Record<string, ClientBreakdownData>;
  state?: ParserClientHighWaterState;
  persistedVersion?: number;
}): ParserHighWaterPlan {
  const supportedVersion = ownValue(SUPPORTED_VERSIONED_PARSERS, args.client);
  // A generation marker without its high-water can exist after an interrupted
  // rollout or state corruption. Re-baselining could re-credit old history, so
  // fail closed until an operator repairs the state.
  if (args.state == null && args.persistedVersion != null) {
    return { mode: "freeze", increments: {} };
  }
  if (args.incomingVersion == null) {
    return args.state ? { mode: "freeze", increments: {} } : { mode: "status-quo", increments: {} };
  }
  if (args.incomingVersion !== supportedVersion) {
    return { mode: "freeze", increments: {} };
  }
  if (!args.fullHistory) {
    const legacyAggregate = aggregateSnapshot(args.existingLegacyDays);
    return args.state || legacyAggregate.tokens > 0
      ? { mode: "freeze", increments: {} }
      : { mode: "status-quo", increments: {} };
  }

  const incomingAggregate = aggregateSnapshot(args.incomingDays);
  if (args.state == null) {
    const legacyAggregate = aggregateSnapshot(args.existingLegacyDays);
    const nextState: ParserClientHighWaterState = {
      version: supportedVersion,
      // Anchor the cumulative cap to both sides. If local history was missing
      // from the first v2 scan, restoring it later must not look like new work.
      aggregate: advanceAggregate(legacyAggregate, incomingAggregate),
      days: args.incomingDays,
    };
    return {
      mode: legacyAggregate.tokens > 0 ? "baseline-legacy" : "baseline-new",
      increments: {},
      nextState,
    };
  }
  if (!validState(args.state, supportedVersion)) {
    return { mode: "freeze", increments: {} };
  }

  return {
    mode: "incremental",
    increments: allocateIncrements(
      args.state.days,
      args.incomingDays,
      args.state.aggregate,
      incomingAggregate
    ),
    nextState: {
      version: supportedVersion,
      aggregate: advanceAggregate(args.state.aggregate, incomingAggregate),
      days: advanceDays(args.state.days, args.incomingDays),
    },
  };
}

export function addClientBreakdownIncrement(
  existing: ClientBreakdownData | undefined,
  increment: ClientBreakdownData
): ClientBreakdownData {
  if (!existing) return increment;
  const models = copyModels(existing.models);
  const represented = breakdownFromModels(models);
  const remainder = emptyModel();
  for (const field of TOKEN_FIELDS) {
    remainder[field] = positive((existing[field] || 0) - represented[field]);
  }
  remainder.tokens = TOKEN_FIELDS.reduce(
    (sum, field) => sum + remainder[field],
    0
  );
  remainder.cost = quantizeCost(
    positive((existing.cost || 0) - represented.cost)
  );
  remainder.messages = positive((existing.messages || 0) - represented.messages);
  if (remainder.tokens > 0 || remainder.cost > 0 || remainder.messages > 0) {
    const remainderModel = existing.modelId || "unknown";
    const prior = { ...(ownValue(models, remainderModel) ?? emptyModel()) };
    for (const field of TOKEN_FIELDS) prior[field] += remainder[field];
    prior.tokens = TOKEN_FIELDS.reduce((sum, field) => sum + prior[field], 0);
    prior.cost = quantizeCost(prior.cost + remainder.cost);
    prior.messages += remainder.messages;
    models[remainderModel] = prior;
  }
  for (const [modelId, delta] of Object.entries(increment.models)) {
    const model = { ...(ownValue(models, modelId) ?? emptyModel()) };
    for (const field of TOKEN_FIELDS) model[field] = (model[field] || 0) + delta[field];
    model.tokens = TOKEN_FIELDS.reduce((sum, field) => sum + model[field], 0);
    model.cost = quantizeCost((model.cost || 0) + delta.cost);
    model.messages = (model.messages || 0) + delta.messages;
    models[modelId] = model;
  }
  const merged = breakdownFromModels(models);
  merged.provenance = {
    ...deriveClientBreakdownProvenance(merged),
    ...(existing.provenance?.costIsComplete === false
      ? { costIsComplete: false }
      : {}),
  };
  return merged;
}
