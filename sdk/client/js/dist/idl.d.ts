import { IDL } from "@icp-sdk/core/candid";
export declare const graphIdlFactory: ({ IDL: LocalIDL }: {
    IDL: typeof IDL;
}) => IDL.ServiceClass<string, {
    query: IDL.FuncClass<[IDL.TextClass, IDL.OptClass<any[][]>], [IDL.VariantClass]>;
    explain: IDL.FuncClass<[IDL.TextClass], [IDL.VariantClass]>;
    update: IDL.FuncClass<[IDL.TextClass, IDL.OptClass<any[][]>], [IDL.VariantClass]>;
    prepare: IDL.FuncClass<[IDL.TextClass, IDL.TextClass, IDL.OptClass<Record<string, any>>], [IDL.VariantClass]>;
    list_prepared_api: IDL.FuncClass<[], [IDL.VariantClass]>;
    execute_prepared_query: IDL.FuncClass<[IDL.TextClass, IDL.VecClass<any[]>, IDL.OptClass<Record<string, any>[]>], [IDL.VariantClass]>;
    execute_prepared_update: IDL.FuncClass<[IDL.TextClass, IDL.VecClass<any[]>], [IDL.VariantClass]>;
    drop_prepared: IDL.FuncClass<[IDL.TextClass], [IDL.VariantClass]>;
}>;
//# sourceMappingURL=idl.d.ts.map