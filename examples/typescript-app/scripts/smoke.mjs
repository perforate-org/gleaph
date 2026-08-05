//! Self-contained smoke test for the generated adapter, run without a live Router.
//!
//! A mock `GraphClient` captures the encoded wire params and returns canned wire rows; the test
//! asserts that the generated adapter encoded the real user values correctly and decoded rows
//! back into real JavaScript types (`bigint`, `Temporal`, `GqlDecimal`, `GqlFloat16`, ...).

import assert from "node:assert/strict";
import { Temporal } from "@js-temporal/polyfill";
import { Principal } from "@icp-sdk/core/principal";
import { GqlDecimal, GqlFloat16, GqlFloat128, toApiValue } from "@gleaph/sdk";
import { withPreparedQueries } from "../generated.ts";

const joinedOn = new Temporal.PlainDate(2024, 1, 15);
const joinedAt = Temporal.Instant.from("2024-01-15T10:30:00Z");
const zonedAt = Temporal.ZonedDateTime.from({
  timeZone: "+09:00",
  year: 2024,
  month: 1,
  day: 15,
  hour: 10,
  minute: 30,
});
const duration = new Temporal.Duration(0, 0, 0, 0, 1, 30);

const calls = [];
const client = {
  async preparedQuery(name, params, sort) {
    calls.push({ name, params, sort });
    switch (name) {
      case "find-users":
        return {
          row_count: 1n,
          phase: null,
          token: null,
          rows: [
            {
              user_name: { Text: "alice" },
              user_id: { Uint64: 7n },
              joined_on: toApiValue(joinedOn, "Date"),
              rating: toApiValue(GqlFloat16.fromNumber(4.5), "Float16"),
            },
          ],
        };
      case "user-account":
        return {
          row_count: 1n,
          phase: null,
          token: null,
          rows: [
            {
              balance: toApiValue(10n ** 40n, "Int256"),
              amount: toApiValue(new GqlDecimal("12.50"), "Decimal"),
              score: toApiValue(GqlFloat128.fromString("1.5"), "Float128"),
              joined_at: toApiValue(joinedAt, "DateTime"),
              zoned_at: toApiValue(zonedAt, "ZonedDateTime"),
              elapsed: toApiValue(duration, "Duration"),
              // A principal text decodes into a real `Principal` instance.
              owner: toApiValue("aaaaa-aa", "Principal"),
              tags: toApiValue(["alpha", "beta"], "List"),
            },
          ],
        };
      default:
        throw new Error(`unexpected prepared call ${name}`);
    }
  },
  async preparedMutate(name, params, clientMutationKey) {
    calls.push({ name, params, clientMutationKey });
    return { row_count: 1n };
  },
};

const prepared = withPreparedQueries(client);

// Prepared read with a sort key: params are encoded as wire values, rows decode to real types.
const users = await prepared.findUsers({ term: "al" }, [
  { key: "user_name", direction: "asc" },
]);
assert.deepEqual(calls[0].params, { term: { Text: "al" } });
assert.deepEqual(calls[0].sort, [{ key: "user_name", direction: "asc" }]);
assert.equal(users.rows.length, 1);
const [user] = users.rows;
assert.equal(user.user_name, "alice");
assert.equal(user.user_id, 7n);
assert.ok(user.joined_on instanceof Temporal.PlainDate);
assert.equal(user.joined_on.year, 2024);
assert.ok(user.rating instanceof GqlFloat16);
assert.equal(user.rating.toNumber(), 4.5);

// Exotic scalar decode: Int256, Decimal, Float128, temporal, Principal, and lists.
const account = await prepared.userAccount({ user_id: 42n });
assert.deepEqual(calls[1].params, { user_id: { Uint64: 42n } });
const [profile] = account.rows;
assert.equal(profile.balance, 10n ** 40n);
assert.ok(profile.amount instanceof GqlDecimal);
assert.ok(profile.amount.equals(new GqlDecimal("12.50")));
assert.ok(profile.score instanceof GqlFloat128);
assert.equal(profile.score.toNumber(), 1.5);
assert.ok(profile.joined_at instanceof Temporal.Instant);
assert.equal(profile.joined_at.epochNanoseconds, joinedAt.epochNanoseconds);
assert.ok(profile.zoned_at instanceof Temporal.ZonedDateTime);
assert.equal(profile.zoned_at.epochNanoseconds, zonedAt.epochNanoseconds);
assert.ok(profile.elapsed instanceof Temporal.Duration);
assert.equal(profile.elapsed.hours, 1);
assert.equal(profile.elapsed.minutes, 30);
assert.ok(profile.owner instanceof Principal);
assert.equal(profile.owner.toText(), "aaaaa-aa");
assert.deepEqual(profile.tags, ["alpha", "beta"]);

// Idempotent mutation: the explicit client mutation key reaches the transport and exotic params
// are encoded into their wire bytes.
const created = await prepared.createUser(
  {
    user_id: 43n,
    user_name: "ada",
    joined_on: joinedOn,
    signup_fee: new GqlDecimal("12.50"),
    score: GqlFloat128.fromString("1.5"),
  },
  "create-user-ada-1",
);
const mutation = calls[2];
assert.equal(mutation.name, "create-user");
assert.equal(mutation.clientMutationKey, "create-user-ada-1");
assert.equal(mutation.params.user_id.Uint64, 43n);
assert.deepEqual(mutation.params.user_name, { Text: "ada" });
assert.deepEqual(mutation.params.joined_on, toApiValue(joinedOn, "Date"));
assert.equal(mutation.params.signup_fee.Decimal.byteLength, 16);
assert.equal(mutation.params.score.Float128.byteLength, 16);
assert.equal(created.row_count, 1n);

console.log("typescript example smoke passed");
