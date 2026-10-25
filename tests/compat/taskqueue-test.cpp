/* SPDX-License-Identifier: LGPL-2.1-or-later */
/* Lifecycle scenarios adapted from OnlyCrayZ-Labs/xgameruntime tests/taskqueue.c
 * at 0975046858cbb146ee4cc88720a54f860fb7d1a5, for Flightdeck's existing queue.
 * This executable uses synthetic callbacks only: no game, account or network.
 */
#include "XTaskQueue.h"
#include <array>
#include <atomic>
#include <cstdio>
#include <thread>
#include <vector>

static std::atomic<unsigned> checks{0}, failures{0};
static void check(bool ok,const char* name) {
    ++checks;
    if(!ok) { ++failures; std::fprintf(stderr,"FAIL %s\n",name); }
}
struct Queue {
    XTaskQueueHandle value=nullptr;
    ~Queue(){if(value)XTaskQueueCloseHandle(value);}
};
struct Event {
    HANDLE value=CreateEventW(nullptr,TRUE,FALSE,nullptr);
    ~Event(){CloseHandle(value);}
};
static void CALLBACK complete(void* context){SetEvent(static_cast<Event*>(context)->value);}
static bool wait(Event& event){return WaitForSingleObject(event.value,10000)==WAIT_OBJECT_0;}
static void pump(XTaskQueueHandle work,XTaskQueueHandle completion,Event& done) {
    ULONGLONG end=GetTickCount64()+5000;
    while(WaitForSingleObject(done.value,0)!=WAIT_OBJECT_0&&GetTickCount64()<end) {
        XTaskQueueDispatch(work,XTaskQueuePort::Work,10);
        XTaskQueueDispatch(completion,XTaskQueuePort::Completion,10);
    }
    check(WaitForSingleObject(done.value,0)==WAIT_OBJECT_0,"termination barrier completes");
}

struct Producer {unsigned next=0;};
struct Item {Producer* owner;unsigned number;};
static void CALLBACK ordered(void* context,BOOLEAN canceled) {
    auto& item=*static_cast<Item*>(context);
    check(!canceled,"manual item not canceled");
    check(item.owner->next++==item.number,"one producer retains FIFO order");
}
static void concurrent_manual() {
    Queue queue;
    check(SUCCEEDED(XTaskQueueCreate(XTaskQueueDispatchMode::Manual,XTaskQueueDispatchMode::Manual,&queue.value)),"create manual queue");
    std::array<Producer,8> owners{};
    std::array<std::array<Item,500>,8> items{};
    std::vector<std::thread> producers;
    for(unsigned p=0;p<owners.size();++p) {
        for(unsigned i=0;i<items[p].size();++i)items[p][i]={&owners[p],i};
        producers.emplace_back([&,p]{for(auto& item:items[p])check(SUCCEEDED(XTaskQueueSubmitCallback(queue.value,XTaskQueuePort::Work,&item,ordered)),"concurrent submission");});
    }
    for(auto& thread:producers)thread.join();
    check(!XTaskQueueDispatch(queue.value,XTaskQueuePort::Completion,0),"manual ports stay separate");
    while(XTaskQueueDispatch(queue.value,XTaskQueuePort::Work,0)){}
    for(auto& owner:owners)check(owner.next==500,"all producer items delivered once");
    Event done;
    check(SUCCEEDED(XTaskQueueTerminate(queue.value,FALSE,&done,complete)),"terminate drained manual queue");
    pump(queue.value,queue.value,done);
}

struct Blocked {Event entered,release,exited;};
static void CALLBACK blocked(void* context,BOOLEAN) {
    auto& state=*static_cast<Blocked*>(context);
    SetEvent(state.entered.value);
    check(wait(state.release),"blocked callback released");
    SetEvent(state.exited.value);
}
static void active_shutdown() {
    for(unsigned repetition=0;repetition<8;++repetition) {
        Queue queue;Blocked callback;Event stopped;
        check(SUCCEEDED(XTaskQueueCreate(XTaskQueueDispatchMode::ThreadPool,XTaskQueueDispatchMode::ThreadPool,&queue.value)),"create pool queue");
        check(SUCCEEDED(XTaskQueueSubmitCallback(queue.value,XTaskQueuePort::Work,&callback,blocked)),"submit active callback");
        check(wait(callback.entered),"pool callback entered");
        std::thread terminator([&]{check(SUCCEEDED(XTaskQueueTerminate(queue.value,TRUE,nullptr,nullptr)),"waiting termination result");SetEvent(stopped.value);});
        check(WaitForSingleObject(stopped.value,30)==WAIT_TIMEOUT,"termination waits for active callback");
        SetEvent(callback.release.value);
        check(wait(stopped),"termination finishes after release");terminator.join();
        check(WaitForSingleObject(callback.exited.value,0)==WAIT_OBJECT_0,"callback exited before waiting termination");
    }
}

static void duplicate_lifetime() {
    Queue queue,alias;Blocked callback;
    check(SUCCEEDED(XTaskQueueCreate(XTaskQueueDispatchMode::ThreadPool,XTaskQueueDispatchMode::Manual,&queue.value)),"create alias owner");
    check(SUCCEEDED(XTaskQueueDuplicateHandle(queue.value,&alias.value)),"duplicate queue ownership");
    XTaskQueueCloseHandle(queue.value);queue.value=nullptr;
    check(SUCCEEDED(XTaskQueueSubmitCallback(alias.value,XTaskQueuePort::Work,&callback,blocked)),"alias survives original close");
    check(wait(callback.entered),"alias callback entered");
    XTaskQueueCloseHandle(alias.value);alias.value=nullptr;
    SetEvent(callback.release.value);
    check(wait(callback.exited),"active callback survives final handle close");
}

struct Count {unsigned count=0;bool canceled=false;};
static void CALLBACK counted(void* context,BOOLEAN canceled) {
    auto& result=*static_cast<Count*>(context);++result.count;result.canceled=canceled;
}
static void composites() {
    Queue work,completion,composite;Count own,borrowed;
    check(SUCCEEDED(XTaskQueueCreate(XTaskQueueDispatchMode::Manual,XTaskQueueDispatchMode::Manual,&work.value)),"create work owner");
    check(SUCCEEDED(XTaskQueueCreate(XTaskQueueDispatchMode::Manual,XTaskQueueDispatchMode::Manual,&completion.value)),"create completion owner");
    XTaskQueuePortHandle work_port=nullptr, completion_port=nullptr;
    check(SUCCEEDED(XTaskQueueGetPort(work.value,XTaskQueuePort::Work,&work_port)),"get work port");
    check(SUCCEEDED(XTaskQueueGetPort(completion.value,XTaskQueuePort::Completion,&completion_port)),"get completion port");
    check(SUCCEEDED(XTaskQueueCreateComposite(work_port,completion_port,&composite.value)),"create composite");
    check(SUCCEEDED(XTaskQueueSubmitCallback(work.value,XTaskQueuePort::Work,&own,counted)),"submit owner item");
    check(SUCCEEDED(XTaskQueueSubmitCallback(composite.value,XTaskQueuePort::Work,&borrowed,counted)),"submit composite item");
    Event done;
    check(SUCCEEDED(XTaskQueueTerminate(composite.value,FALSE,&done,complete)),"terminate composite");
    pump(work.value,completion.value,done);
    check(own.count==1&&!own.canceled,"owner item survives composite termination");
    check(borrowed.count==1&&borrowed.canceled,"only composite item canceled");
    check(SUCCEEDED(XTaskQueueSubmitCallback(work.value,XTaskQueuePort::Work,&own,counted)),"owner still accepts work");
    while(XTaskQueueDispatch(work.value,XTaskQueuePort::Work,0)){}
    check(own.count==2&&!own.canceled,"owner keeps running after composite");
}
static void delayed_shutdown() {
    Queue queue;Count delayed;Event done;
    check(SUCCEEDED(XTaskQueueCreate(XTaskQueueDispatchMode::Manual,XTaskQueueDispatchMode::Manual,&queue.value)),"create delayed queue");
    check(SUCCEEDED(XTaskQueueSubmitDelayedCallback(queue.value,XTaskQueuePort::Work,60000,&delayed,counted)),"submit delayed callback");
    check(!XTaskQueueDispatch(queue.value,XTaskQueuePort::Work,0),"delayed callback not prematurely ready");
    check(SUCCEEDED(XTaskQueueTerminate(queue.value,FALSE,&done,complete)),"terminate pending timer");
    pump(queue.value,queue.value,done);
    check(delayed.count==1&&delayed.canceled,"timer canceled promptly and exactly once");
}
int main() {
    concurrent_manual();active_shutdown();duplicate_lifetime();composites();delayed_shutdown();
    std::printf("SUMMARY taskqueue checks=%u failures=%u\n",checks.load(),failures.load());
    return failures.load()?1:0;
}
