export async function installFakePiAggregate({ eventBus, runtime }) {
  globalThis.__yggPiAggregateShared = { marker: "first" };
  runtime.aggregate.loadOrder.push("first");
  eventBus.on("aggregate:second-ready", (payload) => {
    runtime.aggregate.eventOrder.push(`first-listener:${payload}`);
  });
}
