#ifdef HAVE_STAGED_LIBRARY
extern int stage_dependency(void);
#endif

int main(void)
{
#ifdef HAVE_STAGED_LIBRARY
    return stage_dependency();
#else
    return 0;
#endif
}
