This is a postgres proxy that will allow you to join an uncommitted transaction and perform queries.

The primary use case is during development and debugging. Set a breakpoint in your application/test suite, and then use this proxy to perform queries against the database while the transaction is paused.

## Design
The client application connects through the proxy to postgres. The application operates normally. The proxy exposes a second port that allows a developer to connect and perform queries against the database mid transaction.

```mermaid
graph LR
  A[Client] --Transaction--> B[Proxy]
  B --Transaction--> C[Postgres]
  D[User] --> B
```
When the second port is hit, queries will go over the existing connection. From the perspective of the Postgres transaction, it just receives another query.
```mermaid
sequenceDiagram
    box lightpink Development
      actor User as Developer
      participant Client
    end
    box orange proxy
      participant ProxyClientSocket
      participant ProxyDebugSocket
      participant PostgresConnection
    end
    box lightblue Postgres
      participant Transaction
    end
    User ->+ Client: RUN
    Client ->+ ProxyClientSocket: Connect
    ProxyClientSocket ->+ PostgresConnection: Connect
    Client ->> ProxyClientSocket: Begin
    ProxyClientSocket ->> PostgresConnection: Begin
    PostgresConnection ->>+ Transaction: Begin
    Client ->> ProxyClientSocket: Query
    ProxyClientSocket ->> PostgresConnection: Query
    PostgresConnection ->> Transaction: Query
    Transaction -->> PostgresConnection: Result
    PostgresConnection -->> ProxyClientSocket: Result
    ProxyClientSocket -->> Client: Result
    User -> Client: BREAK
    deactivate Client
    User ->+ ProxyDebugSocket: Connect
    User ->> ProxyDebugSocket: Query
    ProxyDebugSocket ->> PostgresConnection: Query
    PostgresConnection ->> Transaction: Query
    Transaction -->> PostgresConnection: Result
    PostgresConnection -->> ProxyDebugSocket: Result
    ProxyDebugSocket -->> User: Result
    User -> ProxyDebugSocket: Disconnect
    deactivate ProxyDebugSocket
    User ->+ Client: CONTINUE
    Client ->> ProxyClientSocket: Commit
    ProxyClientSocket ->> PostgresConnection: Commit
    PostgresConnection ->> Transaction: Commit
    deactivate Transaction
    Client -> ProxyClientSocket: Disconnect
    ProxyClientSocket -> PostgresConnection: Disconnect
    deactivate PostgresConnection

    deactivate ProxyClientSocket
    Client ->- User: EXIT
```
### State Machines
Internally, we operate via 2 state machine:

#### Client Listener.
When a client connects, we spawn a new forwarder task.
```mermaid
stateDiagram
  Listening --> ClientConnected : Client Connects
  ClientConnected --> Listening : Spawn Forwarder Task
```
#### Forwarder Task
The forwarder task will listen on a new port for debug connections. It will also listen for commands from the client, forward them to the postgres connection, and relay any responses back to the client. The forwarder task will also listen for debug commands, and forward them to the postgres connection, and relay any responses back to the debug client.

While a debug command is being processed, the forwarder task will not process any client commands. 

```mermaid
stateDiagram
  [*] -->  SSL: Receive SSL payload
  SSL --> Authenticated: Receive Authenticate payload
  [*] --> Authenticated: Receive Authenticate payload
  Authenticated --> Listening : Listen for Debug Connections
  Listening --> ForwardingClient : Receive command from client
  ForwardingClient --> ForwardingClient : Client command not finished
  ForwardingClient --> Listening : Finished client command
  Listening --> ForwardingServer : Response received from Server 
  ForwardingServer --> ForwardingServer : Server response not finished
  ForwardingServer --> Listening : Server state is READY_FOR_QUERY
  Listening --> DebugMode : Debug Client Connects
  DebugMode --> DebugForwardingClient : Receive command from Debug Client
  DebugForwardingClient --> DebugForwardingClient : Debug command not finished
  DebugForwardingClient --> DebugMode: Debug command finished
  DebugMode --> DebugForwardingServer : Response received from Server
  DebugForwardingServer --> DebugForwardingServer : Server command not finished
  DebugForwardingServer --> DebugMode : Server state is READY_FOR_QUERY
  DebugMode --> Listening : Debug Client Disconnects
```