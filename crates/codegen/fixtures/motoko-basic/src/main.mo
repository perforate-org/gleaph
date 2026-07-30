import Generated "./generated";

persistent actor {
  transient let executor : Generated.FindUsersExecutor = {
    executeQuery = func(
      _name : Text,
      _params : Generated.FindUsersParams,
    ) : async Generated.PreparedResponse<Generated.FindUsersRow> {
      {
        row_count = 0 : Nat64;
        rows = [] : [Generated.FindUsersRow];
      }
    };
  };

  transient let queries = Generated.FindUsersQueries(executor);

  public func generated_module_smoke() : async Nat64 {
    let response = await queries.execute({ term = "fixture" });
    response.row_count
  };
};
