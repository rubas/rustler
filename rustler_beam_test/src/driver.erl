%% The Erlang half of rustler_beam_test. A test binary starts one `erl` node
%% with this module. It loads the NIF library into a generated module and runs
%% each test NIF in a process of its own.
%%
%% The test binary listens on a TCP port of the loopback interface, and the
%% node connects to it. Frames have a length prefix of 4 bytes:
%%
%%   to the node    <<M:8, Module:M/binary, L:16, Library:L/binary, Nifs/binary>>
%%   to the binary  <<0>> | <<1, Reason/binary>>
%%   to the node    <<Id:32, Test/binary>>
%%   to the binary  <<Id:32, 0>> | <<Id:32, 1, Report/binary>>
%%
%% The first two load the library, and the other two run a test.
-module(rustler_beam_test_driver).
-export([main/1]).

main(Port) ->
    Options = [binary, {packet, 4}, {active, true}, {nodelay, true}],
    {ok, Socket} = gen_tcp:connect({127, 0, 0, 1}, Port, Options),
    receive
        {tcp, Socket, <<M:8, Name:M/binary, L:16, Library:L/binary, Nifs/binary>>} ->
            Module = binary_to_atom(Name),
            case load(Module, Library, [{binary_to_atom(Nif), Arity} || <<N:8, Nif:N/binary, Arity:8>> <= Nifs]) of
                ok ->
                    ok = gen_tcp:send(Socket, <<0>>),
                    serve(Socket, Module, #{});
                {error, Reason} ->
                    ok = gen_tcp:send(Socket, [1, io_lib:format("~tp", [Reason])]),
                    halt(1)
            end
    end.

%% `erlang:load_nif/2` accepts a library only from the module that the library
%% names, and only when that module defines every NIF of the library. So we
%% compile that module, with a stub for each NIF.
load(Module, Library, Nifs) ->
    Path = {string, 1, unicode:characters_to_list(Library)},
    Load = {function, 1, '$load', 0, [{clause, 1, [], [], [call(load_nif, [Path, {integer, 1, 0}])]}]},
    Forms = [
        {attribute, 1, module, Module},
        {attribute, 1, export, [{'$load', 0} | Nifs]},
        Load
        | [stub(Nif) || Nif <- Nifs]
    ],
    {ok, Module, Code} = compile:forms(Forms),
    {module, Module} = code:load_binary(Module, "rustler_beam_test", Code),
    Module:'$load'().

stub({Name, Arity}) ->
    Arguments = lists:duplicate(Arity, {var, 1, '_'}),
    {function, 1, Name, Arity, [{clause, 1, Arguments, [], [call(nif_error, [{atom, 1, not_loaded}])]}]}.

call(Function, Arguments) ->
    {call, 1, {remote, 1, {atom, 1, erlang}, {atom, 1, Function}}, Arguments}.

serve(Socket, Module, Running) ->
    receive
        {tcp, Socket, <<Id:32, Name/binary>>} ->
            Test = binary_to_existing_atom(Name),
            {_, Monitor} = spawn_monitor(fun() -> exit({verdict, run(Module, Test, start)}) end),
            serve(Socket, Module, Running#{Monitor => Id});
        {'DOWN', Monitor, process, _, Exit} ->
            {Id, Rest} = maps:take(Monitor, Running),
            ok = gen_tcp:send(Socket, [<<Id:32>> | verdict(Exit)]),
            serve(Socket, Module, Rest);
        {tcp_closed, Socket} ->
            halt()
    end.

verdict({verdict, ok}) -> [0];
verdict({verdict, {error, Report}}) -> [1, Report];
verdict(Exit) -> [1, io_lib:format("the test process exited with ~tp", [Exit])].

%% A test NIF returns its verdict, or asks for the next message of its process
%% and continues with it in a second call.
run(Module, Test, Step) ->
    case Module:Test(Step) of
        {next_message, Continuation} ->
            receive
                Message -> run(Module, Test, {Continuation, Message})
            after 5000 -> {error, <<"no message arrived within 5 seconds">>}
            end;
        Verdict ->
            Verdict
    end.
