initSidebarItems({"fn":[["alloc","Allocates a buffer of the given length, with an alignment of 1."],["connection_close","Close a connection previously initialized with [`connection_new`]."],["connection_closed","Can be called at any point by the JavaScript code if the connection switches to the `Closed` state."],["connection_message","Notify of a message being received on the connection. The connection must be in the `Open` state."],["connection_new","Must initialize a new connection that tries to connect to the given multiaddress."],["connection_open","Called by the JavaScript code if the connection switches to the `Open` state. The connection must be in the `Opening` state."],["connection_send","Queues data on the given connection. The data is found in the memory of the WebAssembly virtual machine, at the given pointer. The data must be sent as a binary frame."],["init","Initializes the client."],["json_rpc_respond","Client is emitting a response to a previous JSON-RPC request sent using [`json_rpc_send`]. Also used to send subscriptions notifications."],["json_rpc_send","Emit a JSON-RPC request. If the initialization (see [`init`]) hasn't been started or hasn't finished yet, the request will still be queued."],["log","Client is emitting a log entry."],["monotonic_clock_ms","Must return the number of milliseconds that have passed since an arbitrary point in time."],["start_timer","After at least `milliseconds` milliseconds have passed, must call [`timer_finished`] with the `id` passed as parameter."],["throw","Must throw an exception. The message is a UTF-8 string found in the memory of the WebAssembly at offset `message_ptr` and with length `message_len`."],["timer_finished","Must be called in response to [`start_timer`] after the given duration has passed."],["unix_time_ms","Must return the number of milliseconds that have passed since the UNIX epoch, ignoring leap seconds."]]});